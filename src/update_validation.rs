use super::systemd;
use anyhow::{Context, Result};
use log::{error, info};
use std::fs;
use std::path::Path;
use std::process::Command;

static UPDATE_VALIDATION_FILE: &str = "/run/omnect-device-service/omnect_validate_update";
static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";
// ToDo refine configuration for that, e.g as env/config value in /etc/omnect/omnect-device-service.env
static IOT_HUB_DEVICE_UPDATE_SERVICE_START_TIMEOUT_SEC: u64 = 60;
static SYSTEM_IS_RUNNING_TIMEOUT_SEC: u64 = 300;

#[cfg(feature = "bootloader_grub")]
static GRUB_ENV_FILE: &str = "/boot/EFI/BOOT/grubenv";

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

#[cfg(feature = "bootloader_grub")]
async fn finalize_grub() -> Result<()> {
    let list_output = Command::new("grub-editenv")
        .arg(GRUB_ENV_FILE)
        .arg("list")
        .output()
        .with_context(|| "failed to execute 'grub-editenv list'")?;
    anyhow::ensure!(
        list_output.status.success(),
        "grub-editenv list: command returned with error"
    );
    let list_output = String::from_utf8(list_output.stdout)?;
    let list_output = list_output.split('\n');
    let mut omnect_validate_update_part: String = "".to_string();
    for i in list_output {
        let mut j = i.split('=');
        if j.next()
            .with_context(|| "failed to split grub-editenv line")?
            == "omnect_validate_update_part"
        {
            omnect_validate_update_part = j
                .last()
                .with_context(|| "failed to get omnect_validate_update_part value")?
                .to_string();
            break;
        }
    }
    anyhow::ensure!(
        !omnect_validate_update_part.is_empty(),
        "omnect_validate_update_part not set"
    );

    let set_omnect_os_boot = format!("omnect_os_boot={omnect_validate_update_part}");
    anyhow::ensure!(
        Command::new("sudo")
            .args([
                "grub-editenv",
                GRUB_ENV_FILE,
                "set",
                set_omnect_os_boot.as_str()
            ])
            .status()
            .with_context(|| "finalize: failed to set omnect_os_boot")?
            .success(),
        "\"grub-editenv set {set_omnect_os_boot}\" failed"
    );

    anyhow::ensure!(
        Command::new("sudo")
            .args([
                "grub-editenv",
                GRUB_ENV_FILE,
                "unset",
                "omnect_validate_update"
            ])
            .status()
            .with_context(|| "finalize: failed to unset omnect_validate_update")?
            .success(),
        "\"grub-editenv unset omnect_validate_update\" failed"
    );

    anyhow::ensure!(
        Command::new("sudo")
            .args([
                "grub-editenv",
                GRUB_ENV_FILE,
                "unset",
                "omnect_validate_update_part"
            ])
            .status()
            .with_context(|| "finalize: failed to unset omnect_validate_update_part")?
            .success(),
        "\"grub-editenv unset omnect_validate_update_part\" failed"
    );

    Ok(())
}

#[cfg(feature = "bootloader_uboot")]
async fn finalize_uboot() -> Result<()> {
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
            .status()
            .context("finalize: failed to execute 'fw_setenv bootpart'")?
            .success(),
        "\"fw_setenv bootpart {omnect_validate_update_part}\" failed"
    );

    anyhow::ensure!(
        Command::new("sudo")
            .arg("fw_setenv")
            .arg("omnect_validate_update")
            .status()
            .context("finalize: failed to execute 'fw_setenv omnect_validate_update'")?
            .success(),
        "\"fw_setenv omnect_validate_update\" failed"
    );

    anyhow::ensure!(
        Command::new("sudo")
            .arg("fw_setenv")
            .arg("omnect_validate_update_part")
            .status()
            .context("finalize: failed to execute 'fw_setenv omnect_validate_update_part'")?
            .success(),
        "\"fw_setenv omnect_validate_update_part\" failed"
    );

    Ok(())
}

async fn finalize() -> Result<()> {
    #[cfg(feature = "bootloader_uboot")]
    finalize_uboot().await?;

    #[cfg(feature = "bootloader_grub")]
    finalize_grub().await?;

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
        }
        let fin = finalize().await;
        if fin.is_err() {
            error!("finalize error: {:#?}", fin.err());
            systemd::reboot().await?;
        }
    }

    Ok(())
}
