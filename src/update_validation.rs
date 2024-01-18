use super::bootloader_env::bootloader_env::{
    bootloader_env, set_bootloader_env, unset_bootloader_env,
};
use super::systemd;
use crate::systemd::WatchdogHandler;
use anyhow::{bail, ensure, Context, Result};
use log::{debug, info};
use std::{fs, path::Path, time::Duration};

static UPDATE_VALIDATION_FILE: &str = "/run/omnect-device-service/omnect_validate_update";
static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";
// ToDo refine configuration for that, e.g as env/config value in /etc/omnect/omnect-device-service.env
static IOT_HUB_DEVICE_UPDATE_SERVICE_START_TIMEOUT_SEC: u64 = 60;
static SYSTEM_IS_RUNNING_TIMEOUT_SEC: u64 = 300;

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

pub async fn check(wdt: &mut WatchdogHandler) -> Result<()> {
    print_wdt_usec_from_env();
    let secs = Duration::from_secs(90);
    let micros = wdt.interval(secs.as_micros())?;
    std::thread::sleep(Duration::from_secs(5));
    print_wdt_usec_from_env();
    if let Some(micros) = micros {
        let _ = wdt.interval(micros.into())?;
    }
    print_wdt_usec_from_env();


    if let Ok(true) = Path::new(UPDATE_VALIDATION_FILE).try_exists() {
        // prolong watchdog interval for update validation phase
        let secs = Duration::from_secs(90);
        let micros = wdt.interval(secs.as_micros())?;

        if let Err(e) = validate().await {
            systemd::reboot().await?;
            bail!("validate error: {e:#}");
        }

        if let Err(e) = finalize().await {
            systemd::reboot().await?;
            bail!("finalize error: {e:#}");
        }

        if let Some(micros) = micros {
            let _ = wdt.interval(micros.into())?;
        }
    } else {
        info!("no update to be validated")
    }

    Ok(())
}

fn print_wdt_usec_from_env()  {
    if let Ok(usec) = std::env::var("WATCHDOG_USEC") {
        debug!("wdt_usec_from_env: {usec}");
    }
}
