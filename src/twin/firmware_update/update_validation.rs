use crate::{
    bootloader_env,
    common::{RootPartition, from_json_file, to_json_file},
    systemd::{self, unit::UnitAction},
    twin::{firmware_update::common::*, web_service},
};
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{env, fs, path::Path, sync::Arc, time::SystemTime};
use tokio::{
    sync::{RwLock, oneshot},
    time::{Duration, timeout},
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

#[derive(Clone, Debug, Default, Serialize)]
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

#[derive(Clone, Deserialize, Serialize)]
pub struct UpdateValidationParams {
    deadline_timestamp: SystemTime,
    restart_count: u8,
}

#[derive(Default)]
pub struct UpdateValidation {
    params: Option<UpdateValidationParams>,
    tx_cancel_timer: Option<oneshot::Sender<()>>,
    status: Arc<RwLock<UpdateValidationStatus>>,
    local_update: bool,
}

impl UpdateValidation {
    pub async fn new() -> Result<Self> {
        let new_self = match UpdateValidationStatus::init() {
            UpdateValidationStatus::ValidatingTrial(trial) => Self::start_validation(trial == 0)?,
            status => UpdateValidation {
                status: Arc::new(RwLock::new(status)),
                ..Default::default()
            },
        };
        info!("update validation status: {:?}", new_self.status);
        new_self.report().await;
        Ok(new_self)
    }

    fn start_validation(first_start: bool) -> Result<Self> {
        let params = if first_start {
            UpdateValidationParams {
                deadline_timestamp: SystemTime::now()
                    .checked_add(Self::timeout())
                    .context("failed to build deadline timestamp")?,
                restart_count: 0,
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

        // check if there is an update validation config
        let mut local_update = false;
        if let Ok(true) = Path::new(&update_validation_config_path!()).try_exists() {
            local_update =
                from_json_file::<_, UpdateValidationConfig>(update_validation_config_path!())?
                    .local;
        };

        let mut update_validation = UpdateValidation {
            status: Arc::new(RwLock::new(UpdateValidationStatus::ValidatingTrial(
                params.restart_count,
            ))),
            params: Some(params),
            local_update,
            ..Default::default()
        };

        update_validation.start_timeout()?;
        Ok(update_validation)
    }

    pub async fn set_authenticated(&mut self, authenticated: bool) -> Result<()> {
        if matches!(
            *self.status.read().await,
            UpdateValidationStatus::ValidatingTrial(_)
        ) && self.tx_cancel_timer.is_some()
        {
            debug!(
                "authenticated: {authenticated}, local update: {}",
                self.local_update
            );

            // for local updates we accept if there is no connection to iothub
            if self.local_update || authenticated {
                // cancel update validation reboot timer
                if let Err(e) = self
                    .tx_cancel_timer
                    .take()
                    .context("failed to get tx_cancel_timer")?
                    .send(())
                {
                    error!("tx_cancel_timer cannot send: {e:#?}");
                }
            }
        }
        Ok(())
    }

    async fn validate(local_update: bool) -> Result<()> {
        debug!("validate update");

        systemd::wait_for_system_running().await?;

        /* ToDo: if it returns with an error, we may want to handle the state
         * "degraded" and possibly ignore certain failed services via configuration
         */
        info!("system is running");

        // remove iot-hub-device-service barrier file and start service as part of validation
        debug!("starting {IOT_HUB_DEVICE_UPDATE_SERVICE}");
        fs::remove_file(UPDATE_VALIDATION_FILE).context("remove UPDATE_VALIDATION_FILE")?;

        // in case of local update we don't take care of starting deviceupdate-agent.service,
        // since it might fail because of missing iothub connection.
        // instead we let deviceupdate-agent.timer doing the job periodically
        if !local_update {
            systemd::unit::unit_action(
                IOT_HUB_DEVICE_UPDATE_SERVICE,
                UnitAction::Start,
                systemd_zbus::Mode::Fail,
            )
            .await?;
        }

        debug!("successfully started {IOT_HUB_DEVICE_UPDATE_SERVICE}");

        Ok(())
    }

    async fn finalize(status: Arc<RwLock<UpdateValidationStatus>>) -> Result<()> {
        info!("finalize update");
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

        let mut status = status.write().await;
        *status = UpdateValidationStatus::Succeeded;

        Self::report_impl(status.clone()).await;

        Ok(())
    }

    pub async fn report(&self) {
        Self::report_impl(self.status.read().await.clone()).await
    }

    async fn report_impl(status: UpdateValidationStatus) {
        if cfg!(not(feature = "mock")) {
            web_service::publish(
                web_service::PublishChannel::UpdateValidationStatusV1,
                json!({"status": status}),
            )
            .await;
        }
    }

    fn start_timeout(&mut self) -> Result<()> {
        let (tx_cancel_timer, rx_cancel_timer) = oneshot::channel();
        let remaining_time = self
            .params
            .clone()
            .context("validation params missing")?
            .deadline_timestamp
            .duration_since(SystemTime::now())
            .context("failed to build remaining timeout secs")?;
        let status = Arc::clone(&self.status);
        let local_update = self.local_update;
        self.tx_cancel_timer = Some(tx_cancel_timer);
        tokio::spawn(async move {
            info!("observe update with timeout: {}s", remaining_time.as_secs());

            let observe_update = async move {
                // now wait that we get canceled as a result of a successful startup
                if let Err(e) = rx_cancel_timer.await {
                    warn!("observe update validation: {e:#}. Application stopped from outside?");
                    return Ok(());
                }

                Self::validate(local_update).await?;
                Self::finalize(status).await
            };

            let error = match timeout(remaining_time, observe_update).await {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e),
                Err(e) => Some(anyhow::anyhow!(e.to_string())),
            };

            if let Some(e) = error {
                error!("update validation failed: {e:#}");
                if let Err(e) = systemd::reboot("swupdate-validation-failed", &e.to_string()).await
                {
                    error!("failed to trigger reboot: {e:#}");
                }
            }
            info!("update successfully validated")
        });

        Ok(())
    }

    fn timeout() -> Duration {
        let mut timeout = Duration::from_secs(UPDATE_VALIDATION_TIMEOUT_IN_SECS_DEFAULT);
        if let Ok(secs) = env::var("UPDATE_VALIDATION_TIMEOUT_IN_SECS") {
            match secs.parse::<u64>() {
                Ok(secs) => {
                    timeout = Duration::from_secs(secs);
                }
                _ => error!(
                    "ignore invalid confirmation timeout {secs}s and use default {}s",
                    timeout.as_secs()
                ),
            };
        }
        timeout
    }
}
