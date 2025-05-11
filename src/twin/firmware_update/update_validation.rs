use crate::{
    bootloader_env,
    common::{RootPartition, from_json_file, to_json_file},
    reboot_reason,
    systemd::{self, unit::UnitAction, watchdog::WatchdogManager},
    twin::{firmware_update::common::*, web_service},
};
use anyhow::{Context, Result, bail};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{env, fs, path::Path};
use tokio::{
    sync::oneshot,
    time::{Duration, Instant, timeout_at},
};

// this file is used to detect if we have to validate an update
static UPDATE_VALIDATION_FILE: &str = "/run/omnect-device-service/omnect_validate_update";
// this file is used to signal others that the update validation is successful, by deleting it
static UPDATE_VALIDATION_COMPLETE_BARRIER_FILE: &str =
    "/run/omnect-device-service/omnect_validate_update_complete_barrier";
// this file is used to determine a recovery after a failed update validation
static UPDATE_VALIDATION_FAILED_FILE: &str =
    "/run/omnect-device-service/omnect_validate_update_failed";
static UPDATE_VALIDATION_TIMEOUT_IN_SECS_DEFAULT: u64 = 300;

#[derive(Debug, Default, Serialize)]
enum UpdateValidationStatus {
    #[default]
    NoUpdate,
    ValidatingTrial(u8),
    Recovered,
    Succeeded,
}

impl UpdateValidationStatus {
    fn init() -> Self {
        if let Ok(true) = Path::new(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE).try_exists() {
            UpdateValidationStatus::ValidatingTrial(1)
        } else if let Ok(true) = Path::new(UPDATE_VALIDATION_FILE).try_exists() {
            UpdateValidationStatus::ValidatingTrial(0)
        } else if let Ok(true) = Path::new(UPDATE_VALIDATION_FAILED_FILE).try_exists() {
            UpdateValidationStatus::Recovered
        } else {
            UpdateValidationStatus::NoUpdate
        }
    }
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
    status: UpdateValidationStatus,
}

impl UpdateValidation {
    pub async fn new(tx_validated: oneshot::Sender<()>) -> Result<Self> {
        let new_self = match UpdateValidationStatus::init() {
            UpdateValidationStatus::ValidatingTrial(trial) => {
                Self::start_validation(trial == 0, tx_validated)?
            }
            status => {
                if let Err(e) = tx_validated.send(()) {
                    error!("failed to send validated state: {e:#?}")
                }
                UpdateValidation {
                    status,
                    ..Default::default()
                }
            }
        };
        info!("update validation status: {:?}", new_self.status);
        new_self.report().await;
        Ok(new_self)
    }

    fn start_validation(first_start: bool, tx_validated: oneshot::Sender<()>) -> Result<Self> {
        let params = if first_start {
            let start_boottime_secs = sysinfo::System::boot_time();
            let deadline_boottime_secs = start_boottime_secs + Self::timeout_secs();

            // check if there is an update validation config
            let mut local_update = false;
            if let Ok(true) = Path::new(&update_validation_config_path!()).try_exists() {
                let config: UpdateValidationConfig =
                    from_json_file(update_validation_config_path!())?;
                local_update = config.local;
            };

            UpdateValidationParams {
                start_boottime_secs,
                deadline_boottime_secs,
                local_update,
                ..Default::default()
            }
        } else {
            // we detected update validation before, but were not validated before
            let mut params: UpdateValidationParams =
                from_json_file(UPDATE_VALIDATION_COMPLETE_BARRIER_FILE)?;
            params.restart_count += 1;
            params
        };

        to_json_file(
            &params,
            UPDATE_VALIDATION_COMPLETE_BARRIER_FILE,
            first_start,
        )?;

        let mut update_validation = UpdateValidation {
            status: UpdateValidationStatus::ValidatingTrial(params.restart_count),
            params: Some(params),
            tx_validated: Some(tx_validated),
            ..Default::default()
        };

        update_validation.start_timeout()?;
        Ok(update_validation)
    }

    pub async fn set_authenticated(&mut self, authenticated: bool) -> Result<()> {
        if matches!(self.status, UpdateValidationStatus::ValidatingTrial(_)) {
            let params = self.params.as_mut().context("validation params missing")?;

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

        systemd::wait_for_system_running().await?;

        /* ToDo: if it returns with an error, we may want to handle the state
         * "degraded" and possibly ignore certain failed services via configuration
         */
        info!("system is running");

        // remove iot-hub-device-service barrier file and start service as part of validation
        debug!("starting {IOT_HUB_DEVICE_UPDATE_SERVICE}");
        fs::remove_file(UPDATE_VALIDATION_FILE).context("remove UPDATE_VALIDATION_FILE")?;

        let params = self.params.as_mut().context("validation params missing")?;

        // in case of local update we don't take care of starting deviceupdate-agent.service,
        // since it might fail because of missing iothub connection.
        // instead we let deviceupdate-agent.timer doing the job periodically
        if !params.local_update {
            systemd::unit::unit_action(
                IOT_HUB_DEVICE_UPDATE_SERVICE,
                UnitAction::Start,
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
        // prolong watchdog interval for update validation phase by twice the remaining
        // update cancellation timer
        let saved_interval = WatchdogManager::interval(
            self.remaining_timeout()?
                .checked_mul(2)
                .context("watchdog duration overflow")?,
        )
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

    fn start_timeout(&mut self) -> Result<()> {
        let (tx_cancel_timer, rx_cancel_timer) = oneshot::channel();
        let deadline = Instant::now() + self.remaining_timeout()?;
        self.tx_cancel_timer = Some(tx_cancel_timer);
        tokio::spawn(async move {
            info!("reboot timer started");
            match timeout_at(deadline, rx_cancel_timer).await {
                Err(_) => {
                    error!("update validation timed out: write reboot reason and reboot");
                    if let Err(e) = reboot_reason::write_reboot_reason(
                        "swupdate-validation-failed",
                        "timer expired",
                    ) {
                        error!(
                            "update validation timed out: failed to write reboot reason with {e:#}"
                        );
                    }
                    if let Err(e) = systemd::reboot().await {
                        error!("update validation timed out: failed to trigger reboot with {e:#}");
                    }
                }
                _ => info!("reboot timer canceled."),
            }
        });

        Ok(())
    }

    fn timeout_secs() -> u64 {
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

    fn remaining_timeout(&self) -> Result<Duration> {
        let params = self.params.as_ref().context("validation params missing")?;

        Ok(
            Duration::from_secs(params.start_boottime_secs + Self::timeout_secs())
                - Duration::from_secs(sysinfo::System::boot_time()),
        )
    }
}
