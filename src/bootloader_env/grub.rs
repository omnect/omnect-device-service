use anyhow::{Context, Result, ensure};
use std::process::Command;

use super::SUDO_BIN;

static GRUB_EDITENV_BIN: &str = "/usr/bin/grub-editenv";
static GRUB_ENV_FILE: &str = "/boot/EFI/BOOT/grubenv";
static GRUB_CMD_LIST: &str = "list";
static GRUB_CMD_SET: &str = "set";
static GRUB_CMD_UNSET: &str = "unset";

pub fn bootloader_env(key: &str) -> Result<String> {
    let list = Command::new(GRUB_EDITENV_BIN)
        .arg(GRUB_ENV_FILE)
        .arg(GRUB_CMD_LIST)
        .output()
        .context("failed to execute 'grub-editenv list'")?;
    ensure!(
        list.status.success(),
        "grub-editenv list: command returned with error"
    );
    let list = String::from_utf8(list.stdout)?;
    let list = list.split('\n');
    let mut value = "".to_string();
    for i in list {
        if let Some((k, v)) = i.split_once('=')
            && k == key
        {
            value = v.trim().to_string();
            break;
        }
    }
    Ok(value)
}

pub fn set_bootloader_env(key: &str, value: &str) -> Result<()> {
    let set = format!("{key}={value}");
    ensure!(
        Command::new(SUDO_BIN)
            .args([GRUB_EDITENV_BIN, GRUB_ENV_FILE, GRUB_CMD_SET, set.as_str()])
            .status()
            .context(format!("failed to call \"sudo grub-editenv set {set}\""))?
            .success(),
        "\"sudo grub-editenv set {set}\" failed"
    );

    Ok(())
}

pub fn unset_bootloader_env(key: &str) -> Result<()> {
    ensure!(
        Command::new(SUDO_BIN)
            .args([GRUB_EDITENV_BIN, GRUB_ENV_FILE, GRUB_CMD_UNSET, key])
            .status()
            .context(format!("failed to call \"sudo grub-editenv unset {key}\""))?
            .success(),
        "\"sudo grub-editenv unset {key}\" failed"
    );

    Ok(())
}
