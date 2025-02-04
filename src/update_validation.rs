use crate::{
    bootloader_env, systemd, systemd::unit::UnitAction, systemd::watchdog::WatchdogManager,
    reboot_reason,
};
use anyhow::{bail, ensure, Context, Result};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DurationMilliSeconds};
use std::{env, fs, fs::OpenOptions, path::Path};
use tokio::{
    sync::oneshot,
    task::JoinHandle,
    time::{timeout, Duration},
};

// this file is used to detect if we have to validate an update
static UPDATE_VALIDATION_FILE: &str = "/run/omnect-device-service/omnect_validate_update";
// this file is used to signal others that the update validation is successful, by deleting it
static UPDATE_VALIDATION_COMPLETE_BARRIER_FILE: &str =
    "/run/omnect-device-service/omnect_validate_update_complete_barrier";
static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";
static UPDATE_VALIDATION_TIMEOUT_IN_SECS: u64 = 300;

#[serde_as]
#[derive(Default, Deserialize, Serialize)]
pub struct UpdateValidation {
    #[serde_as(as = "DurationMilliSeconds<u64>")]
    #[serde(rename = "start_monotonic_time_ms")]
    start_monotonic_time: Duration,
    restart_count: u8,
    authenticated: bool,
    #[serde(skip)]
    run_update_validation: bool,
    #[serde(skip)]
    validation_timeout: Duration,
    #[serde(skip)]
    tx: Option<oneshot::Sender<()>>,
    #[serde(skip)]
    join_handle: Option<JoinHandle<()>>,
}

impl UpdateValidation {
    pub fn new() -> Result<Self> {
        let mut new_self = UpdateValidation::default();
        let validation_timeout = Duration::from_secs(UPDATE_VALIDATION_TIMEOUT_IN_SECS);
        if let Ok(timeout_secs) = env::var("UPDATE_VALIDATION_TIMEOUT_IN_SECS") {
            match timeout_secs.parse::<u64>() {
                Ok(timeout_secs) => {
                    new_self.validation_timeout = Duration::from_secs(timeout_secs);
                }
                _ => error!("ignore invalid confirmation timeout {timeout_secs}"),
            };
        }

        if let Ok(true) = Path::new(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE).try_exists() {
            // we detected update validation before, but were not validated before
            new_self = serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE)
                    .context(format!(
                        "retry read of {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"
                    ))?,
            )
            .context(format!(
                "deserializing of UpdateValidation from {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"
            ))?;
            new_self.restart_count += 1;
            info!("retry start ({})", new_self.restart_count);
            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE)
                    .context(format!("retry write of {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"))?,
                    &new_self,
            )
            .context(
                format!("retry serializing of UpdateValidation to {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"),
            )?;
            let now = std::time::Duration::from(nix::time::clock_gettime(
                nix::time::ClockId::CLOCK_MONOTONIC,
            )?);
            new_self.validation_timeout =
                validation_timeout - (now - new_self.start_monotonic_time);
            new_self.run_update_validation = true;
        } else if let Ok(true) = Path::new(UPDATE_VALIDATION_FILE).try_exists() {
            info!("first start");
            new_self.start_monotonic_time = std::time::Duration::from(nix::time::clock_gettime(
                nix::time::ClockId::CLOCK_MONOTONIC,
            )?);

            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE)
                    .context(format!("first write of {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"))?,
                &new_self,
            )
            .context(
                format!("first serializing of UpdateValidation to {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"),
            )?;
            new_self.validation_timeout = validation_timeout;
            new_self.run_update_validation = true;
        } else {
            info!("no update to be validated");
            new_self.run_update_validation = false;
        }

        if new_self.run_update_validation {
            let (tx, rx) = oneshot::channel();
            new_self.tx = Some(tx);
            let validation_timeout = new_self.validation_timeout;

            new_self.join_handle = Some(tokio::spawn(async move {
		let timeout_ms = validation_timeout.as_millis();
                info!(
                    "reboot timer started ({timeout_ms} ms)."
                );
                match timeout(validation_timeout, rx).await {
                    Err(_) => {
                        error!("update validation: timeout; write reboot reason and reboot ...");
			if let Err(e) = reboot_reason::reboot_reason(
			    "swupdate-validation-failed", &format!("timer ({timeout_ms} ms) expired")) {
                            error!("update validation: timer: failed to write reboot reason [{e}]");
			}
                        if let Err(_e) = systemd::reboot().await {
                            error!("update validation: timer couldn't trigger reboot");
			}
                    }
                    _ => info!("reboot timer canceled."),
                }
            }));
        }
        Ok(new_self)
    }

    pub async fn set_authenticated(&mut self) -> Result<()> {
        if !self.run_update_validation {
            return Ok(());
        }

        self.authenticated = true;
        debug!("status set to \"authenticated\"");

        serde_json::to_writer_pretty(
            OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE)
                .context(format!("authenticated: write of {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"))?,
            &self,
        )
        .context(
            format!("authenticated: serializing of UpdateValidation to {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"),
        )?;

        // for now start validation blocking twin::init - maybe we want an successful twin::init as part of validation at some point?
        self.check().await
    }

    async fn validate(&mut self) -> Result<()> {
        debug!("started");
        let now = std::time::Duration::from(nix::time::clock_gettime(
            nix::time::ClockId::CLOCK_MONOTONIC,
        )?);
        let timeout = self.validation_timeout - (now - self.start_monotonic_time);
        systemd::wait_for_system_running(timeout).await?;

        /* ToDo: if it returns with an error, we may want to handle the state
         * "degrated" and possibly ignore certain failed services via configuration
         */
        info!("system is running");

        // remove iot-hub-device-service barrier file and start service as part of validation
        debug!("starting deviceupdate-agent.service");
        fs::remove_file(UPDATE_VALIDATION_FILE).context("remove UPDATE_VALIDATION_FILE")?;

        let now = std::time::Duration::from(nix::time::clock_gettime(
            nix::time::ClockId::CLOCK_MONOTONIC,
        )?);
        let timeout = self.validation_timeout - (now - self.start_monotonic_time);

        systemd::unit::unit_action(IOT_HUB_DEVICE_UPDATE_SERVICE, UnitAction::Start, timeout)
            .await?;
        debug!("successfully started iot-hub-device-update");

        info!("successfully validated update");
        Ok(())
    }

    async fn finalize(&mut self) -> Result<()> {
        let omnect_validate_update_part = bootloader_env::get("omnect_validate_update_part")?;
        ensure!(
            !omnect_validate_update_part.is_empty(),
            "update validation: omnect_validate_update_part not set"
        );
        bootloader_env::set("omnect_os_bootpart", omnect_validate_update_part.as_str())?;
        bootloader_env::unset("omnect_validate_update")?;
        bootloader_env::unset("omnect_validate_update_part")?;

        fs::remove_file(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE).context(format!(
            "update validation: remove {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"
        ))?;
        // cancel update validation reboot timer
        if let Err(e) = self.tx.take().unwrap().send(()) {
            error!(
                "update validation: could not cancel update validation reboot timer: {:#?}",
                e
            );
        }

        Ok(())
    }

    pub async fn check(&mut self) -> Result<()> {
        // prolong watchdog interval for update validation phase
        let saved_interval = WatchdogManager::interval(self.validation_timeout).await?;

        if let Err(e) = self.validate().await {
	    if let Err(er) = reboot_reason::reboot_reason(
		"swupdate-validation-failed", &format!("validate error: {e:#}")) {
                error!("update validation: validate: failed to write reboot reason [{er}]");
	    }
            systemd::reboot().await?;
            bail!("update validation: validate error: {e:#}");
        }
        if let Err(e) = self.finalize().await {
	    if let Err(er) = reboot_reason::reboot_reason(
		"swupdate-validation-failed", &format!("finalize error: {e:#}")) {
                error!("update validation: finalize: failed to write reboot reason [{er}]");
	    }
            systemd::reboot().await?;
            bail!("update validation: finalize error: {e:#}");
        }

        if let Some(interval) = saved_interval {
            let _ = WatchdogManager::interval(interval).await?;
        }

        Ok(())
    }
}
