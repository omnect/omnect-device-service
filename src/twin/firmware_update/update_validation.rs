use crate::{
    bootloader_env, reboot_reason,
    systemd::{self, unit::UnitAction, watchdog::WatchdogManager},
    twin::{firmware_update::common::*, system_info::RootPartition, web_service},
};
use anyhow::{bail, Context, Result};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
// this file is used to determine a recovery after a failed update validation
static UPDATE_VALIDATION_FAILED: &str = "/run/omnect-device-service/omnect_validate_update_failed";
static UPDATE_VALIDATION_TIMEOUT_IN_SECS: u64 = 300;

#[derive(Default, Serialize)]
enum UpdateValidationStatus {
    #[default]
    NoUpdate,
    ValidatingTrial(u8),
    Recovered,
    Succeeded,
}

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
    validation_timeout: Duration,
    #[serde(skip)]
    tx_validated: Option<oneshot::Sender<()>>,
    #[serde(skip)]
    tx_cancel_timer: Option<oneshot::Sender<()>>,
    #[serde(skip)]
    join_handle: Option<JoinHandle<()>>,
    #[serde(skip)]
    status: UpdateValidationStatus,
}

impl UpdateValidation {
    pub async fn new(tx_validated: oneshot::Sender<()>) -> Result<Self> {
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
            new_self = from_json_file(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE)?;
            new_self.restart_count += 1;
            new_self.status = UpdateValidationStatus::ValidatingTrial(new_self.restart_count);
            info!("retry start ({})", new_self.restart_count);
            to_json_file(&new_self, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, false)?;
            let now = Duration::from(nix::time::clock_gettime(
                nix::time::ClockId::CLOCK_MONOTONIC,
            )?);
            new_self.validation_timeout =
                validation_timeout - (now - new_self.start_monotonic_time);
        } else if let Ok(true) = Path::new(UPDATE_VALIDATION_FILE).try_exists() {
            info!("first start");
            new_self.start_monotonic_time = Duration::from(nix::time::clock_gettime(
                nix::time::ClockId::CLOCK_MONOTONIC,
            )?);
            // check if there is an update validation config
            if let Ok(true) = Path::new(&update_validation_config_path!()).try_exists() {
                let config: UpdateValidationConfig =
                    from_json_file(update_validation_config_path!())?;
                new_self.local_update = config.local;
            }

            to_json_file(&new_self, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, true)?;

            new_self.validation_timeout = validation_timeout;
            new_self.status = UpdateValidationStatus::ValidatingTrial(new_self.restart_count);
        } else if let Ok(true) = Path::new(UPDATE_VALIDATION_FAILED).try_exists() {
            info!("recovered after update validation failed");
            new_self.status = UpdateValidationStatus::Recovered;
            if let Err(e) = &tx_validated.send(()) {
                error!("failed to send validated state: {e:#?}")
            }
            new_self.report().await;
            return Ok(new_self);
        } else {
            info!("no update to be validated");
            new_self.status = UpdateValidationStatus::NoUpdate;
            if let Err(e) = &tx_validated.send(()) {
                error!("failed to send validated state: {e:#?}")
            }
            new_self.report().await;
            return Ok(new_self);
        }

        if matches!(new_self.status, UpdateValidationStatus::ValidatingTrial(_)) {
            let (tx_cancel_timer, rx_cancel_timer) = oneshot::channel();
            new_self.tx_cancel_timer = Some(tx_cancel_timer);
            let validation_timeout = new_self.validation_timeout;

            new_self.join_handle = Some(tokio::spawn(async move {
                let timeout_ms = validation_timeout.as_millis();
                info!("reboot timer started ({timeout_ms} ms).");
                match timeout(validation_timeout, rx_cancel_timer).await {
                    Err(_) => {
                        error!("update validation timed out: write reboot reason and reboot");
                        if let Err(e) = reboot_reason::write_reboot_reason(
                            "swupdate-validation-failed",
                            &format!("timer ({timeout_ms} ms) expired"),
                        ) {
                            error!("update validation timed out: failed to write reboot reason with {e:#}");
                        }
                        if let Err(e) = systemd::reboot().await {
                            error!("update validation timed out: failed to trigger reboot with {e:#}");
                        }
                    }
                    _ => info!("reboot timer canceled."),
                }
            }));
        }
        new_self.tx_validated = Some(tx_validated);
        new_self.report().await;
        Ok(new_self)
    }

    pub async fn set_authenticated(&mut self, authenticated: bool) -> Result<()> {
        if matches!(self.status, UpdateValidationStatus::ValidatingTrial(_)) {
            self.authenticated = authenticated;
            debug!(
                "authenticated: {}, local update: {}",
                self.authenticated, self.local_update
            );

            // for local updates we accept if there is no connection to iothub
            if self.local_update || self.authenticated {
                to_json_file(&self, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, false)?;
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
         * "degraded" and possibly ignore certain failed services via configuration
         */
        info!("system is running");

        // remove iot-hub-device-service barrier file and start service as part of validation
        debug!("starting {IOT_HUB_DEVICE_UPDATE_SERVICE}");
        fs::remove_file(UPDATE_VALIDATION_FILE).context("remove UPDATE_VALIDATION_FILE")?;

        let now = Duration::from(nix::time::clock_gettime(
            nix::time::ClockId::CLOCK_MONOTONIC,
        )?);
        let timeout = self.validation_timeout - (now - self.start_monotonic_time);

        // in case of local update we don't take care of starting deviceupdate-agent.service,
        // since it might fail because of missing iothub connection.
        // instead we let deviceupdate-agent.timer doing the job periodically
        if !self.local_update {
            systemd::unit::unit_action(
                IOT_HUB_DEVICE_UPDATE_SERVICE,
                UnitAction::Start,
                timeout,
                systemd_zbus::Mode::Fail,
            )
            .await?;
        }

        debug!("successfully started {IOT_HUB_DEVICE_UPDATE_SERVICE}");

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
        if let Err(e) = self
            .tx_cancel_timer
            .take()
            .context("failed to get tx_cancel_timer")?
            .send(())
        {
            error!("update validation: could not cancel update validation reboot timer: {e:#?}");
        }

        self.status = UpdateValidationStatus::Succeeded;

        if let Err(e) = self
            .tx_validated
            .take()
            .context("failed to get tx_validated")?
            .send(())
        {
            error!("failed to send validated state: {e:#?}")
        }

        self.report().await;

        Ok(())
    }

    async fn check(&mut self) -> Result<()> {
        // prolong watchdog interval for update validation phase
        let saved_interval = WatchdogManager::interval(self.validation_timeout).await?;

        if let Err(e) = self.validate().await {
            if let Err(e) = reboot_reason::write_reboot_reason(
                "swupdate-validation-failed",
                &format!("validate error: {e:#}"),
            ) {
                error!("check (validate): failed to write reboot reason [{e:#}]");
            }
            systemd::reboot().await?;
            bail!("update validation: validate failed with {e:#}");
        }
        if let Err(e) = self.finalize().await {
            if let Err(e) = reboot_reason::write_reboot_reason(
                "swupdate-validation-failed",
                &format!("finalize error: {e:#}"),
            ) {
                error!("check (finalize): failed to write reboot reason [{e:#}]");
            }
            systemd::reboot().await?;
            bail!("update validation: finalize error: {e:#}");
        }

        if let Some(interval) = saved_interval {
            let _ = WatchdogManager::interval(interval).await?;
        }

        Ok(())
    }

    pub async fn report(&self) {
        web_service::publish(
            web_service::PublishChannel::UpdateValidationStatus,
            json!({"status": self.status}),
        )
        .await;
    }
}
