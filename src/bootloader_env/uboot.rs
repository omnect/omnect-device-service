use anyhow::{Context, Result, bail, ensure};
use std::process::Command;

pub fn bootloader_env(key: &str) -> Result<String> {
    let value = Command::new("/usr/bin/sudo")
        .arg("/usr/bin/fw_printenv")
        .arg(key)
        .output()?;
    if !value.status.success() {
        bail!("fw_printenv {key} failed");
    }
    let value = String::from_utf8(value.stdout)?;
    let mut value = value
        .split_once('=')
        .context(format!("failed to get {key}'s value"))?
        .1
        .trim()
        .to_string();
    let len = value.trim_end_matches(&['\r', '\n'][..]).len();
    value.truncate(len);

    Ok(value)
}

pub fn set_bootloader_env(key: &str, value: &str) -> Result<()> {
    ensure!(
        Command::new("/usr/bin/sudo")
            .args(["/usr/bin/fw_setenv_no_script.sh", key, value])
            .status()
            .context(format!("failed to execute 'fw_setenv {key} {value}'"))?
            .success(),
        "\"fw_setenv {key} {value}\" failed"
    );

    Ok(())
}

pub fn unset_bootloader_env(key: &str) -> Result<()> {
    ensure!(
        Command::new("/usr/bin/sudo")
            .args(["/usr/bin/fw_setenv", key])
            .status()
            .context(format!("failed to execute \"fw_setenv {key}\""))?
            .success(),
        "\"fw_setenv {key}\" failed"
    );

    Ok(())
}
