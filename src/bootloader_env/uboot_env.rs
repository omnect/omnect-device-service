use anyhow::{bail, ensure, Context, Result};
use std::process::Command;

pub fn bootloader_env(key: &str) -> Result<String> {
    let value = Command::new("sudo").arg("fw_printenv").arg(key).output()?;
    if !value.status.success() {
        bail!("fw_printenv {key} failed");
    }
    let value = String::from_utf8(value.stdout)?;
    let mut value = value
        .split('=')
        .last()
        .with_context(|| "failed to get {key}'s value")?
        .trim()
        .to_string();
    let len = value.trim_end_matches(&['\r', '\n'][..]).len();
    value.truncate(len);

    Ok(value)
}

pub fn set_bootloader_env(key: &str, value: &str) -> Result<()> {
    ensure!(
        Command::new("sudo")
            .args(["fw_setenv", key, value])
            .status()
            .with_context(|| "failed to execute 'fw_setenv {key} {value}'")?
            .success(),
        "\"fw_setenv {key} {value}\" failed"
    );

    Ok(())
}

pub fn unset_bootloader_env(key: &str) -> Result<()> {
    ensure!(
        Command::new("sudo")
            .args(["fw_setenv", key])
            .status()
            .with_context(|| "failed to execute \"fw_setenv {key}\"")?
            .success(),
        "\"fw_setenv {key}\" failed"
    );

    Ok(())
}
