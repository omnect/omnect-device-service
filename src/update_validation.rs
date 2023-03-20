use super::systemd;
use anyhow::{Context, Result};
use log::info;
use std::fs;
use std::path::Path;
use std::process::Command;

static UPDATE_VALIDATION_FILE: &str = "/run/omnect-device-service/omnect_validate_update";
static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";

fn validate() -> Result<()> {
    /*
     * For now the only validation is a successful module provisioning plus
     * a successful start of iot-hub-device-update.
     * Successful provisioning is ensured by calling this function once on
     * authentication.
     */

    // remove iot-hub-device-service barrier file and start service as part of validation
    fs::remove_file(UPDATE_VALIDATION_FILE).context("remove UPDATE_VALIDATION_FILE")?;
    systemd::start_unit(IOT_HUB_DEVICE_UPDATE_SERVICE)?;

    info!("Successfully validated Update.");
    Ok(())
}

fn finalize() -> Result<()> {
    let omnect_validate_update_part = Command::new("sudo")
        .arg("fw_printenv")
        .arg("omnect_validate_update_part")
        .output()?;
    if !omnect_validate_update_part.status.success() {
        anyhow::bail!("fw_printenv omnect_validate_update_part failed");
    }
    let omnect_validate_update_part = String::from_utf8(omnect_validate_update_part.stdout)?;
    let omnect_validate_update_part = match omnect_validate_update_part.split('=').last() {
        Some(omnect_validate_update_part) => omnect_validate_update_part.trim(),
        None => anyhow::bail!("omnect_validate_update_part split failed"),
    };

    anyhow::ensure!(
        Command::new("sudo")
            .args(["fw_setenv", "bootpart", omnect_validate_update_part])
            .status()?
            .success(),
        "\"fw_setenv bootpart {omnect_validate_update_part}\" failed"
    );

    anyhow::ensure!(
        Command::new("sudo")
            .arg("fw_setenv")
            .arg("omnect_validate_update")
            .status()?
            .success(),
        "\"fw_setenv omnect_validate_update\" failed"
    );

    anyhow::ensure!(
        Command::new("sudo")
            .arg("fw_setenv")
            .arg("omnect_validate_update_part")
            .status()?
            .success(),
        "\"fw_setenv omnect_validate_update_part\" failed"
    );

    Ok(())
}

pub fn check() -> Result<()> {
    /*
     * ToDo: as soon as we can switch to rust >=1.63 we should use
     * Path::try_exists() here
     */
    if Path::new(UPDATE_VALIDATION_FILE).exists() && (validate().is_err() || finalize().is_err()) {
        info!("update validation failed... reboot");
        return systemd::reboot().map_err(|e| anyhow::anyhow!("update validation reboot: {e}"));
    }

    Ok(())
}
