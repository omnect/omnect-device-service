use super::bootloader_env::bootloader_env::{
    bootloader_env, set_bootloader_env, unset_bootloader_env,
};
use super::systemd;
use anyhow::{bail, ensure, Context, Result};
use log::{error, info};
use std::fs;
use std::path::Path;

static UPDATE_VALIDATION_FILE: &str = "/run/omnect-device-service/omnect_validate_update";
static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";
// ToDo refine configuration for that, e.g as env/config value in /etc/omnect/omnect-device-service.env
static IOT_HUB_DEVICE_UPDATE_SERVICE_START_TIMEOUT_SEC: u64 = 60;
static SYSTEM_IS_RUNNING_TIMEOUT_SEC: u64 = 300;

async fn validate() -> Result<()> {
    info!("update validation started");
    systemd::wait_for_system_running(SYSTEM_IS_RUNNING_TIMEOUT_SEC).await?;

    /* ToDo: if it returns with an error, we may want to handle the state
     * "degrated" and possibly ignore certain failed services via configuration
     */
    info!("system is running");

    // remove iot-hub-device-service barrier file and start service as part of validation
    info!("starting deviceupdate-agent.service");
    fs::remove_file(UPDATE_VALIDATION_FILE).context("remove UPDATE_VALIDATION_FILE")?;

    systemd::start_unit(
        IOT_HUB_DEVICE_UPDATE_SERVICE_START_TIMEOUT_SEC,
        IOT_HUB_DEVICE_UPDATE_SERVICE,
    )
    .await?;
    info!("successfully started iot-hub-device-update");

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
    /*
     * ToDo: as soon as we can switch to rust >=1.63 we should use
     * Path::try_exists() here
     */
    if Path::new(UPDATE_VALIDATION_FILE).exists() {
        let val = validate().await;
        if val.is_err() {
            error!("validate error: {:#?}", val.err());
            systemd::reboot().await?;
            bail!("validate failed");
        }
        let fin = finalize().await;
        if fin.is_err() {
            error!("finalize error: {:#?}", fin.err());
            systemd::reboot().await?;
            bail!("finalize failed");
        }
    }

    Ok(())
}
