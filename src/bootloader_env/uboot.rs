use anyhow::{Context, Result, bail, ensure};
use std::process::Command;

static SUDO_BIN: &str = "/usr/bin/sudo";
static FW_PRINTENV_BIN: &str = "/usr/bin/fw_printenv";
static FW_SETENV_NO_SCRIPT_BIN: &str = "/usr/bin/fw_setenv_no_script.sh";
static FW_SETENV_BIN: &str = "/usr/bin/fw_setenv";

pub fn bootloader_env(key: &str) -> Result<String> {
    let value = Command::new(SUDO_BIN)
        .arg(FW_PRINTENV_BIN)
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
        Command::new(SUDO_BIN)
            .args([FW_SETENV_NO_SCRIPT_BIN, key, value])
            .status()
            .context(format!("failed to execute 'fw_setenv {key} {value}'"))?
            .success(),
        "\"fw_setenv {key} {value}\" failed"
    );

    Ok(())
}

pub fn unset_bootloader_env(key: &str) -> Result<()> {
    ensure!(
        Command::new(SUDO_BIN)
            .args([FW_SETENV_BIN, key])
            .status()
            .context(format!("failed to execute \"fw_setenv {key}\""))?
            .success(),
        "\"fw_setenv {key}\" failed"
    );

    Ok(())
}
