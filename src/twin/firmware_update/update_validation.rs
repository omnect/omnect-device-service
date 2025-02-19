use super::common::*;
use crate::{
    bootloader_env,
    systemd::{self, unit::UnitAction, watchdog::WatchdogManager},
    twin::system_info::RootPartition,
};
use anyhow::{bail, Context, Result};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DurationMilliSeconds};
use std::{env, fs, path::Path};
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
static UPDATE_VALIDATION_TIMEOUT_IN_SECS: u64 = 300;

#[serde_as]
#[derive(Default, Deserialize, Serialize)]
pub struct UpdateValidation {
    #[serde_as(as = "DurationMilliSeconds<u64>")]
    #[serde(rename = "start_monotonic_time_ms")]
    start_monotonic_time: Duration,
    restart_count: u8,
    authenticated: bool,
    local_update: bool,
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
            new_self = json_from_file(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE)?;
            new_self.restart_count += 1;
            info!("retry start ({})", new_self.restart_count);
            json_to_file(&new_self, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, false)?;
            let now = Duration::from(nix::time::clock_gettime(
                nix::time::ClockId::CLOCK_MONOTONIC,
            )?);
            new_self.validation_timeout =
                validation_timeout - (now - new_self.start_monotonic_time);
            new_self.run_update_validation = true;
        } else if let Ok(true) = Path::new(UPDATE_VALIDATION_FILE).try_exists() {
            info!("first start");
            new_self.start_monotonic_time = Duration::from(nix::time::clock_gettime(
                nix::time::ClockId::CLOCK_MONOTONIC,
            )?);

            // check if there is an update validation config
            if let Ok(true) = Path::new(&update_validation_config_path!()).try_exists() {
                let config: UpdateValidationConfig =
                    json_from_file(update_validation_config_path!())?;
                new_self.local_update = config.local;
            }

            json_to_file(&new_self, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, true)?;

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
                info!(
                    "reboot timer started ({} ms).",
                    validation_timeout.as_millis()
                );
                match timeout(validation_timeout, rx).await {
                    Err(_) => {
                        error!("update validation: timeout. rebooting ...");

                        if let Err(e) = systemd::reboot().await {
                            error!("reboot timer couldn't trigger reboot: {e:#}");
                        }
                    }
                    _ => info!("reboot timer canceled."),
                }
            }));
        }
        Ok(new_self)
    }

    pub async fn set_authenticated(&mut self, authenticated: bool) -> Result<()> {
        if self.run_update_validation {
            self.authenticated = authenticated;
            debug!(
                "authenticated: {}, local update: {}",
                self.authenticated, self.local_update
            );

            if self.local_update || self.authenticated {
                json_to_file(&self, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, false)?;
                // for now start validation blocking twin::init - maybe we want an successful twin::init as part of validation at some point?
                return self.check().await;
            }
        }
        Ok(())
    }

    async fn validate(&mut self) -> Result<()> {
        debug!("started");
        let now = Duration::from(nix::time::clock_gettime(
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

        let now = Duration::from(nix::time::clock_gettime(
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
        let omnect_validate_update_part =
            RootPartition::from_index_string(bootloader_env::get("omnect_validate_update_part")?)?;

        bootloader_env::set(
            "omnect_os_bootpart",
            &omnect_validate_update_part.index().to_string(),
        )?;
        bootloader_env::unset("omnect_validate_update")?;
        bootloader_env::unset("omnect_validate_update_part")?;

        fs::remove_file(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE).context(format!(
            "update validation: remove {UPDATE_VALIDATION_COMPLETE_BARRIER_FILE}"
        ))?;

        let _ = fs::remove_file(update_validation_config_path!());

        // cancel update validation reboot timer
        if let Err(e) = self.tx.take().unwrap().send(()) {
            error!(
                "update validation: could not cancel update validation reboot timer: {:#?}",
                e
            );
        }

        Ok(())
    }

    async fn check(&mut self) -> Result<()> {
        // prolong watchdog interval for update validation phase
        let saved_interval = WatchdogManager::interval(self.validation_timeout).await?;

        if let Err(e) = self.validate().await {
            systemd::reboot().await?;
            bail!("update validation: validate error: {e:#}");
        }
        if let Err(e) = self.finalize().await {
            systemd::reboot().await?;
            bail!("update validation: finalize error: {e:#}");
        }

        if let Some(interval) = saved_interval {
            let _ = WatchdogManager::interval(interval).await?;
        }

        Ok(())
    }
}
