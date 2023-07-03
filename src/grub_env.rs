use anyhow::{ensure, Context, Result};
use std::process::Command;

static GRUB_ENV_FILE: &str = "/boot/EFI/BOOT/grubenv";

pub fn bootloader_env(key: &str) -> Result<String> {
    let list = Command::new("grub-editenv")
        .arg(GRUB_ENV_FILE)
        .arg("list")
        .output()
        .with_context(|| "failed to execute 'grub-editenv list'")?;
    ensure!(
        list.status.success(),
        "grub-editenv list: command returned with error"
    );
    let list = String::from_utf8(list.stdout)?;
    let list = list.split('\n');
    let mut value: String = "".to_string();
    for i in list {
        let mut j = i.split('=');
        if j.next()
            .with_context(|| "failed to split grub-editenv line")?
            == key
        {
            value = j
                .last()
                .with_context(|| "failed to get {key}'s value")?
                .trim()
                .to_string();
            break;
        }
    }
    Ok(value)
}

pub fn set_bootloader_env(key: &str, value: &str) -> Result<()> {
    let set = format!("{key}={value}");
    ensure!(
        Command::new("sudo")
            .args(["grub-editenv", GRUB_ENV_FILE, "set", set.as_str()])
            .status()
            .with_context(|| "failed to call \"sudo grub-editenv set {set}\"")?
            .success(),
        "\"sudo grub-editenv set {set}\" failed"
    );

    Ok(())
}

pub fn unset_bootloader_env(key: &str) -> Result<()> {
    ensure!(
        Command::new("sudo")
            .args(["grub-editenv", GRUB_ENV_FILE, "unset", key])
            .status()
            .with_context(|| "failed to call \"sudo grub-editenv unset {key}\"")?
            .success(),
        "\"sudo grub-editenv unset {key}\" failed"
    );

    Ok(())
}
