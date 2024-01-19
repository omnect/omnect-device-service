use super::bootloader_env::bootloader_env::{
    bootloader_env, set_bootloader_env, unset_bootloader_env,
};
use super::systemd;
use crate::systemd::WatchdogManager;
use anyhow::{bail, ensure, Context, Result};
use log::{debug, error, info};
use std::{env, fs, path::Path, time::Duration};

static UPDATE_VALIDATION_FILE: &str = "/run/omnect-device-service/omnect_validate_update";
static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";
// ToDo refine configuration for that, e.g as env/config value in /etc/omnect/omnect-device-service.env
static IOT_HUB_DEVICE_UPDATE_SERVICE_START_TIMEOUT_SEC: u64 = 60;
static SYSTEM_IS_RUNNING_TIMEOUT_SEC: u64 = 300;
static DEFAULT_UPDATE_VALIDATION_TIME_IN_S: u64 = 90;
static ENV_UPDATE_VALIDATION_TIME: &str = "UPDATE_VALIDATION_TIME_IN_S";

async fn validate() -> Result<()> {
    debug!("update validation started");
    systemd::wait_for_system_running(SYSTEM_IS_RUNNING_TIMEOUT_SEC).await?;

    /* ToDo: if it returns with an error, we may want to handle the state
     * "degrated" and possibly ignore certain failed services via configuration
     */
    info!("system is running");

    // remove iot-hub-device-service barrier file and start service as part of validation
    debug!("starting deviceupdate-agent.service");
    fs::remove_file(UPDATE_VALIDATION_FILE).context("remove UPDATE_VALIDATION_FILE")?;

    systemd::start_unit(
        IOT_HUB_DEVICE_UPDATE_SERVICE_START_TIMEOUT_SEC,
        IOT_HUB_DEVICE_UPDATE_SERVICE,
    )
    .await?;
    debug!("successfully started iot-hub-device-update");

    info!("successfully validated update");
    Ok(())
}

async fn finalize() -> Result<()> {
    let omnect_validate_update_part = bootloader_env("omnect_validate_update_part")?;
    ensure!(
        !omnect_validate_update_part.is_empty(),
        "omnect_validate_update_part not set"
    );
    set_bootloader_env("omnect_os_bootpart", omnect_validate_update_part.as_str())?;
    unset_bootloader_env("omnect_validate_update")?;
    unset_bootloader_env("omnect_validate_update_part")?;

    Ok(())
}

pub async fn check() -> Result<()> {
    if let Ok(true) = Path::new(UPDATE_VALIDATION_FILE).try_exists() {
        // prolong watchdog interval for update validation phase
        let mut validation_secs = None;

        if let Ok(secs) = env::var(ENV_UPDATE_VALIDATION_TIME) {
            if let Ok(secs) = secs.parse::<u64>() {
                validation_secs = Some(secs);
                debug!("set update validation time to {secs}s")
            }
            else {
                error!("cannot parse {ENV_UPDATE_VALIDATION_TIME}={secs}")
            }
        }

        let validation_secs = validation_secs.unwrap_or_else(|| {
            debug!("set default update validation timeout to {DEFAULT_UPDATE_VALIDATION_TIME_IN_S}s");
            DEFAULT_UPDATE_VALIDATION_TIME_IN_S
        });

        let saved_interval_micros = WatchdogManager::interval(Duration::from_secs(validation_secs).as_micros())?;

        if let Err(e) = validate().await {
            systemd::reboot().await?;
            bail!("validate error: {e:#}");
        }

        if let Err(e) = finalize().await {
            systemd::reboot().await?;
            bail!("finalize error: {e:#}");
        }

        if let Some(micros) = saved_interval_micros {
            let _ = WatchdogManager::interval(micros.into())?;
        }
    } else {
        info!("no update to be validated")
    }

    Ok(())
}
