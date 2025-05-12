use crate::{
    bootloader_env, reboot_reason,
    systemd::{self, unit::UnitAction, watchdog::WatchdogManager},
    twin::{firmware_update::common::*, system_info::RootPartition, web_service},
};
use anyhow::{bail, Context, Result};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{env, fs, path::Path};
use tokio::{
    sync::oneshot,
    task::JoinHandle,
    time::{timeout_at, Duration, Instant},
};

// this file is used to detect if we have to validate an update
static UPDATE_VALIDATION_FILE: &str = "/run/omnect-device-service/omnect_validate_update";
// this file is used to signal others that the update validation is successful, by deleting it
static UPDATE_VALIDATION_COMPLETE_BARRIER_FILE: &str =
    "/run/omnect-device-service/omnect_validate_update_complete_barrier";
// this file is used to determine a recovery after a failed update validation
static UPDATE_VALIDATION_FAILED: &str = "/run/omnect-device-service/omnect_validate_update_failed";
static UPDATE_VALIDATION_TIMEOUT_IN_SECS_DEFAULT: u64 = 300;
static UPDATE_VALIDATION_DEADLINE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

#[derive(Default, Serialize)]
enum UpdateValidationStatus {
    #[default]
    NoUpdate,
    ValidatingTrial(u8),
    Recovered,
    Succeeded,
}

#[derive(Default, Deserialize, Serialize)]
pub struct UpdateValidationParams {
    start_boottime_secs: u64,
    deadline_boottime_secs: u64,
    restart_count: u8,
    authenticated: bool,
    local_update: bool,
}

#[derive(Default)]
pub struct UpdateValidation {
    params: Option<UpdateValidationParams>,
    tx_validated: Option<oneshot::Sender<()>>,
    tx_cancel_timer: Option<oneshot::Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
    status: UpdateValidationStatus,
}

impl UpdateValidation {
    pub async fn new(tx_validated: oneshot::Sender<()>) -> Result<Self> {
        let mut new_self = UpdateValidation::default();

        if let Ok(true) = Path::new(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE).try_exists() {
            // we detected update validation before, but were not validated before
            let mut params: UpdateValidationParams =
                from_json_file(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE)?;
            params.restart_count += 1;
            info!("retry start ({})", params.restart_count);
            to_json_file(&params, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, false)?;
            let timeout = Self::remaining_timeout(&params.start_boottime_secs)?;
            let _deadline = *UPDATE_VALIDATION_DEADLINE.get_or_init(|| Instant::now() + timeout);
            new_self.status = UpdateValidationStatus::ValidatingTrial(params.restart_count);
            new_self.params = Some(params);
        } else if let Ok(true) = Path::new(UPDATE_VALIDATION_FILE).try_exists() {
            info!("first start");

            let start_boottime_secs: u64 =
                nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)?.tv_sec() as u64;
            let deadline_boottime_secs = start_boottime_secs + Self::configured_timeout();
            let timeout = Self::remaining_timeout(&start_boottime_secs)?;
            let _deadline = *UPDATE_VALIDATION_DEADLINE.get_or_init(|| Instant::now() + timeout);

            // check if there is an update validation config
            let mut local_update = false;
            if let Ok(true) = Path::new(&update_validation_config_path!()).try_exists() {
                let config: UpdateValidationConfig =
                    from_json_file(update_validation_config_path!())?;
                local_update = config.local;
            };

            let params = UpdateValidationParams {
                start_boottime_secs,
                deadline_boottime_secs,
                local_update,
                ..Default::default()
            };

            to_json_file(&params, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, true)?;

            new_self.params = Some(params);
            new_self.status = UpdateValidationStatus::ValidatingTrial(0);
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
            let deadline = *UPDATE_VALIDATION_DEADLINE.get().context("context")?;
            new_self.tx_cancel_timer = Some(tx_cancel_timer);
            new_self.join_handle = Some(tokio::spawn(async move {
                info!("reboot timer started");
                match timeout_at(deadline, rx_cancel_timer).await {
                    Err(_) => {
                        error!("update validation timed out: write reboot reason and reboot");
                        if let Err(e) = reboot_reason::write_reboot_reason(
                            "swupdate-validation-failed",
                            "timer expired",
                        ) {
                            error!("update validation timed out: failed to write reboot reason with {e:#}");
                        }
                        if let Err(e) = systemd::reboot().await {
                            error!(
                                "update validation timed out: failed to trigger reboot with {e:#}"
                            );
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
            let Some(params) = self.params.as_mut() else {
                bail!("validation params missing")
            };
            params.authenticated = authenticated;
            debug!(
                "authenticated: {}, local update: {}",
                params.authenticated, params.local_update
            );

            // for local updates we accept if there is no connection to iothub
            if params.local_update || params.authenticated {
                to_json_file(&params, UPDATE_VALIDATION_COMPLETE_BARRIER_FILE, false)?;
                // for now start validation blocking twin::init - maybe we want an successful twin::init as part of validation at some point?
                return self.check().await;
            }
        }
        Ok(())
    }

    async fn validate(&mut self) -> Result<()> {
        debug!("started");

        timeout_at(
            *UPDATE_VALIDATION_DEADLINE.get().context("context")?,
            systemd::wait_for_system_running(),
        )
        .await??;

        /* ToDo: if it returns with an error, we may want to handle the state
         * "degraded" and possibly ignore certain failed services via configuration
         */
        info!("system is running");

        // remove iot-hub-device-service barrier file and start service as part of validation
        debug!("starting {IOT_HUB_DEVICE_UPDATE_SERVICE}");
        fs::remove_file(UPDATE_VALIDATION_FILE).context("remove UPDATE_VALIDATION_FILE")?;

        let Some(params) = self.params.as_mut() else {
            bail!("validation params missing")
        };

        // in case of local update we don't take care of starting deviceupdate-agent.service,
        // since it might fail because of missing iothub connection.
        // instead we let deviceupdate-agent.timer doing the job periodically
        if !params.local_update {
            systemd::unit::unit_action_with_deadline(
                IOT_HUB_DEVICE_UPDATE_SERVICE,
                UnitAction::Start,
                systemd_zbus::Mode::Fail,
                *UPDATE_VALIDATION_DEADLINE.get().context("context")?,
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
        let Some(params) = &self.params else {
            bail!("validation params missing")
        };
        // prolong watchdog interval for update validation phase
        let saved_interval =
            WatchdogManager::interval(Self::remaining_timeout(&params.start_boottime_secs)?)
                .await?;

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
            web_service::PublishChannel::UpdateValidationStatusV1,
            json!({"status": self.status}),
        )
        .await;
    }

    fn configured_timeout() -> u64 {
        let mut timeout_secs = UPDATE_VALIDATION_TIMEOUT_IN_SECS_DEFAULT;
        if let Ok(secs) = env::var("UPDATE_VALIDATION_TIMEOUT_IN_SECS") {
            match secs.parse::<u64>() {
                Ok(secs) => {
                    timeout_secs = secs;
                }
                _ => error!("ignore invalid confirmation timeout {secs}"),
            };
        }
        timeout_secs
    }

    fn remaining_timeout(start_boottime_secs: &u64) -> Result<Duration> {
        let timeout_secs = Self::configured_timeout();

        let Ok(now_boottime_secs) = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME)
        else {
            bail!("ignore");
        };

        Ok(Duration::from_secs(start_boottime_secs + timeout_secs)
            - Duration::from(now_boottime_secs))
    }
}
