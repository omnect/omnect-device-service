use crate::{
    bootloader_env,
    common::{RootPartition, from_json_file, to_json_file},
    systemd::{self, unit::UnitAction},
    twin::{firmware_update::common::*, web_service},
};
use anyhow::{Context, Result, ensure};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    env, fs,
    future::Future,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Instant, SystemTime},
};
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
static SYSTEM_HEALTHY_DEADLINE_MARGIN_IN_SECS: u64 = 30;

// validation only starts after authentication, so the health check gets the
// time that is actually left: the margin keeps its descriptive error ahead of
// the generic validation timeout, which decides the reboot reason
fn health_deadline(remaining: Duration) -> Duration {
    remaining.saturating_sub(Duration::from_secs(SYSTEM_HEALTHY_DEADLINE_MARGIN_IN_SECS))
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum ValidationStage {
    Authentication = 0,
    SystemHealth = 1,
    EnableAduAgent = 2,
    Finalize = 3,
}

impl ValidationStage {
    // an unknown value falls back to the first stage: claiming a late stage
    // would point the reader away from the one that actually hung
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::SystemHealth,
            2 => Self::EnableAduAgent,
            3 => Self::Finalize,
            _ => Self::Authentication,
        }
    }

    // ends up in extra_info, which is written to a fixed size pmsg record
    // shared by several reboot reasons, so keep it short
    fn timeout_message(&self) -> &'static str {
        match self {
            Self::Authentication => "timeout waiting for authentication",
            Self::SystemHealth => "timeout waiting for system health",
            Self::EnableAduAgent => "timeout enabling adu agent",
            Self::Finalize => "timeout finalizing update",
        }
    }
}

// the validation runs in a spawned task, so its current stage must be readable
// from the outside to tell a timeout what was still pending
#[derive(Clone)]
struct StageTracker(Arc<AtomicU8>);

impl StageTracker {
    fn new() -> Self {
        Self(Arc::new(AtomicU8::new(
            ValidationStage::Authentication as u8,
        )))
    }

    fn set(&self, stage: ValidationStage) {
        self.0.store(stage as u8, Ordering::Relaxed);
    }

    fn get(&self) -> ValidationStage {
        ValidationStage::from_u8(self.0.load(Ordering::Relaxed))
    }
}

async fn observe_with_stage_timeout(
    duration: Duration,
    stage: StageTracker,
    observe: impl Future<Output = Result<()>>,
) -> Result<()> {
    match timeout(duration, observe).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("{}", stage.get().timeout_message()),
    }
}

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
    feature_init_failed: bool,
}

impl UpdateValidation {
    pub async fn new(feature_init_failed: bool) -> Result<Self> {
        let new_self = match UpdateValidationStatus::init() {
            UpdateValidationStatus::ValidatingTrial(trial) => {
                Self::start_validation(trial == 0, feature_init_failed)?
            }
            status => UpdateValidation {
                status: Arc::new(RwLock::new(status)),
                ..Default::default()
            },
        };
        info!("update validation status: {:?}", new_self.status);
        new_self.report().await;
        Ok(new_self)
    }

    fn start_validation(first_start: bool, feature_init_failed: bool) -> Result<Self> {
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
            feature_init_failed,
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

    async fn wait_for_healthy(deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        systemd::wait_for_system_healthy(health_deadline(remaining)).await?;

        info!("system is healthy");

        Ok(())
    }

    async fn enable_adu_agent(local_update: bool) -> Result<()> {
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
            RootPartition::from_index_string(bootloader_env::get(OMNECT_VALIDATE_UPDATE_PART)?)?;
        bootloader_env::set(
            OMNECT_OS_BOOTPART,
            &omnect_validate_update_part.index().to_string(),
        )?;

        Self::finalize_bootargs()?;

        bootloader_env::unset(OMNECT_VALIDATE_UPDATE)?;
        bootloader_env::unset(OMNECT_VALIDATE_UPDATE_PART)?;

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
        web_service::publish(
            web_service::PublishChannel::UpdateValidationStatusV1,
            json!({"status": status}),
        )
        .await;
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
        // the timeout below is monotonic, so the health deadline must be too:
        // a clock step during boot must not shorten it
        let deadline = Instant::now() + remaining_time;
        let status = Arc::clone(&self.status);
        let local_update = self.local_update;
        let feature_init_failed = self.feature_init_failed;
        let stage = StageTracker::new();
        let observe_stage = stage.clone();
        self.tx_cancel_timer = Some(tx_cancel_timer);
        tokio::spawn(async move {
            info!("observe update with timeout: {}s", remaining_time.as_secs());

            let observe_update = async move {
                // the new image may have broken the input of a feature
                ensure!(!feature_init_failed, "feature init failed");

                // now wait that we get canceled as a result of a successful startup
                if let Err(e) = rx_cancel_timer.await {
                    warn!("observe update validation: {e:#}. Application stopped from outside?");
                    return Ok(());
                }

                observe_stage.set(ValidationStage::SystemHealth);
                Self::wait_for_healthy(deadline).await?;

                observe_stage.set(ValidationStage::EnableAduAgent);
                Self::enable_adu_agent(local_update).await?;

                observe_stage.set(ValidationStage::Finalize);
                Self::finalize(status).await
            };

            let error = observe_with_stage_timeout(remaining_time, stage, observe_update)
                .await
                .err();

            if let Some(e) = error {
                error!("update validation failed: {e:#}");
                if let Err(e) = systemd::reboot("swupdate-validation-failed", &e.to_string()).await
                {
                    error!("failed to trigger reboot: {e:#}");
                }
            } else {
                info!("update successfully validated")
            }
        });

        Ok(())
    }

    fn finalize_bootargs() -> Result<()> {
        let omnect_validate_extra_bootargs =
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS).unwrap_or_default();

        if omnect_validate_extra_bootargs == NOARGS_SENTINEL {
            bootloader_env::unset(OMNECT_EXTRA_BOOTARGS)?;
            bootloader_env::unset(OMNECT_VALIDATE_EXTRA_BOOTARGS)?;
        } else if !omnect_validate_extra_bootargs.is_empty() {
            bootloader_env::set(OMNECT_EXTRA_BOOTARGS, &omnect_validate_extra_bootargs)?;
            bootloader_env::unset(OMNECT_VALIDATE_EXTRA_BOOTARGS)?;
        }
        // else empty omnect_validate_extra_bootargs -> no change to omnect_extra_bootargs

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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::bootloader_env::TEST_LOCK as BOOTARGS_TEST_LOCK;

    const TEST_TIMEOUT: Duration = Duration::from_millis(50);

    // run into the timeout while the given stage is pending and return what the
    // reboot reason would report
    async fn timeout_message_for(stage: ValidationStage) -> String {
        let tracker = StageTracker::new();
        let observe_stage = tracker.clone();
        let pending = async move {
            observe_stage.set(stage);
            std::future::pending::<()>().await;
            Ok(())
        };

        observe_with_stage_timeout(TEST_TIMEOUT, tracker, pending)
            .await
            .expect_err("expected a timeout")
            .to_string()
    }

    #[tokio::test]
    async fn timeout_reports_pending_authentication() {
        assert_eq!(
            timeout_message_for(ValidationStage::Authentication).await,
            "timeout waiting for authentication"
        );
    }

    #[tokio::test]
    async fn timeout_reports_pending_system_health() {
        assert_eq!(
            timeout_message_for(ValidationStage::SystemHealth).await,
            "timeout waiting for system health"
        );
    }

    #[tokio::test]
    async fn timeout_reports_pending_adu_agent_enable() {
        assert_eq!(
            timeout_message_for(ValidationStage::EnableAduAgent).await,
            "timeout enabling adu agent"
        );
    }

    #[tokio::test]
    async fn timeout_reports_pending_finalize() {
        assert_eq!(
            timeout_message_for(ValidationStage::Finalize).await,
            "timeout finalizing update"
        );
    }

    const VALIDATION_TIMEOUT: Duration = Duration::from_secs(60);

    // the observe task drops the cancel receiver once it stops waiting for authentication
    async fn stops_waiting_for_authentication(feature_init_failed: bool) -> bool {
        let mut update_validation = UpdateValidation {
            params: Some(UpdateValidationParams {
                deadline_timestamp: SystemTime::now() + VALIDATION_TIMEOUT,
                restart_count: 0,
            }),
            status: Arc::new(RwLock::new(UpdateValidationStatus::ValidatingTrial(0))),
            feature_init_failed,
            ..Default::default()
        };
        update_validation.start_timeout().expect("start timeout");

        let mut tx_cancel_timer = update_validation
            .tx_cancel_timer
            .take()
            .expect("cancel timer");
        timeout(TEST_TIMEOUT, tx_cancel_timer.closed())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn failed_feature_init_fails_validation() {
        assert!(stops_waiting_for_authentication(true).await);
    }

    #[tokio::test]
    async fn validation_waits_for_authentication() {
        assert!(!stops_waiting_for_authentication(false).await);
    }

    #[tokio::test]
    async fn without_timeout_the_observed_result_is_kept() {
        let tracker = StageTracker::new();
        let err = observe_with_stage_timeout(TEST_TIMEOUT, tracker, async {
            anyhow::bail!("some validation error")
        })
        .await
        .expect_err("expected the observed error");

        assert_eq!(err.to_string(), "some validation error");
    }

    #[test]
    fn stage_tracker_starts_at_authentication() {
        assert_eq!(StageTracker::new().get(), ValidationStage::Authentication);
    }

    #[test]
    fn unknown_stage_value_falls_back_to_authentication() {
        assert_eq!(
            ValidationStage::from_u8(ValidationStage::Finalize as u8 + 1),
            ValidationStage::Authentication
        );
    }

    #[test]
    fn health_deadline_keeps_margin_to_validation_timeout() {
        let remaining = Duration::from_secs(UPDATE_VALIDATION_TIMEOUT_IN_SECS_DEFAULT);
        assert_eq!(
            health_deadline(remaining),
            remaining - Duration::from_secs(SYSTEM_HEALTHY_DEADLINE_MARGIN_IN_SECS)
        );
    }

    #[test]
    fn health_deadline_without_slack_is_zero() {
        assert_eq!(
            health_deadline(Duration::from_secs(SYSTEM_HEALTHY_DEADLINE_MARGIN_IN_SECS)),
            Duration::ZERO
        );
        assert_eq!(health_deadline(Duration::ZERO), Duration::ZERO);
    }

    // a low configured timeout must not produce a deadline beyond it, else the
    // generic timeout wins the race and the reboot reason loses the cause
    #[test]
    fn health_deadline_never_exceeds_remaining_time() {
        for secs in [1, 10, SYSTEM_HEALTHY_DEADLINE_MARGIN_IN_SECS, 90, 1200] {
            let remaining = Duration::from_secs(secs);
            assert!(
                health_deadline(remaining) < remaining || remaining.is_zero(),
                "deadline for {secs}s remaining is not shorter than the remaining time"
            );
        }
    }

    #[test]
    fn finalize_bootargs_noargs_sentinel_unsets_both_keys() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").expect("set extra");
        bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, NOARGS_SENTINEL).expect("set validate");

        UpdateValidation::finalize_bootargs().expect("finalize_bootargs");

        assert!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS)
                .expect("get extra")
                .is_empty(),
            "expected omnect_extra_bootargs to be unset"
        );
        assert!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate")
                .is_empty(),
            "expected omnect_validate_extra_bootargs to be unset"
        );
    }

    #[test]
    fn finalize_bootargs_real_value_promotes_and_cleans_up() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").expect("set extra");
        bootloader_env::set(
            OMNECT_VALIDATE_EXTRA_BOOTARGS,
            "console=ttyS0,115200 loglevel=7",
        )
        .expect("set validate");

        UpdateValidation::finalize_bootargs().expect("finalize_bootargs");

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra"),
            "console=ttyS0,115200 loglevel=7"
        );
        assert!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate")
                .is_empty(),
            "expected omnect_validate_extra_bootargs to be unset"
        );
    }

    #[test]
    fn finalize_bootargs_absent_validate_key_leaves_extra_untouched() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "existing_args").expect("set extra");
        // OMNECT_VALIDATE_EXTRA_BOOTARGS intentionally not set

        UpdateValidation::finalize_bootargs().expect("finalize_bootargs");

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra"),
            "existing_args",
            "expected omnect_extra_bootargs to remain unchanged"
        );
    }
}
