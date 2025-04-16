// reboot reason handling

use anyhow::{Context, Result};
use log::warn;
use regex_lite::Regex;

#[cfg(not(feature = "mock"))]
static REBOOT_REASON_SCRIPT: &str = "/usr/sbin/omnect_reboot_reason.sh";
static REBOOT_REASON_DIR_REGEX: &str = r"^(\d+)\+\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2}$";
static REBOOT_REASON_FILE_NAME: &str = "reboot-reason.json";

macro_rules! reboot_reason_dir_path {
    () => {{
        static REBOOT_REASON_DIR_PATH_DEFAULT: &'static str = "/var/lib/omnect/reboot-reason/";
        std::env::var("REBOOT_REASON_DIR_PATH")
            .unwrap_or(REBOOT_REASON_DIR_PATH_DEFAULT.to_string())
    }};
}

#[cfg(not(feature = "mock"))]
pub fn write_reboot_reason(reason: &str, extra_info: &str) -> Result<()> {
    use std::process::Command;

    // make arguments shell script proof
    let reboot_reason_cmd = "log";
    let reason = reason.replace("\"", "'").to_string();
    let extra_info = extra_info.replace("\"", "'").to_string();

    let common_args = [reboot_reason_cmd, &reason, &extra_info];
    let mut cmd: Command;
    // we need to pass sudo only for EFI machines which correlates to feature
    // bootloader_grub
    if cfg!(feature = "bootloader_grub") {
        cmd = Command::new("sudo");
        cmd.args([REBOOT_REASON_SCRIPT]);
    } else if cfg!(feature = "bootloader_uboot") {
        cmd = Command::new(REBOOT_REASON_SCRIPT);
    } else {
        unreachable!()
    };

    anyhow::ensure!(
        cmd.args(common_args)
            .status()
            .context("failed to invoke '{REBOOT_REASON_SCRIPT} {reason} \"{extra_info}\"'")?
            .success(),
        "'{REBOOT_REASON_SCRIPT} {reason} \"{extra_info}\"' failed"
    );

    Ok(())
}

pub fn current_reboot_reason() -> Option<serde_json::Value> {
    // use a closure here to be able to use anyhow::Context for error messages
    // and convert the result to an Option (incl. possibly inspecting errors) afterwards
    let current_reboot_reason = || -> Result<serde_json::Value> {
        let regex = Regex::new(REBOOT_REASON_DIR_REGEX)
            .context("failed to create regex for reboot reason folder")?;
        let dir = std::fs::read_dir(reboot_reason_dir_path!())
            .context("failed to read reboot reason directory")?;
        let dir = dir
            .flatten()
            .filter(|f| {
                if let Ok(m) = f.metadata() {
                    return m.is_dir();
                }
                false
            })
            .max_by_key(|k| {
                let name = k.file_name();
                let name = name.as_os_str().to_str()?;

                if let Some(c) = regex.captures(name) {
                    return c[1].to_string().parse::<u32>().ok();
                };
                None
            })
            .context("failed to identify current reboot reason folder")?;

        let json: serde_json::Value = serde_json::from_reader(
            std::fs::OpenOptions::new()
                .read(true)
                .create(false)
                .open(dir.path().join(REBOOT_REASON_FILE_NAME))
                .context("failed to open reboot reason file")?,
        )
        .context("failed to parse json from reboot reason file")?;

        Ok(json
            .get("reboot_reason")
            .context("failed to get reboot_reason from json")?
            .clone())
    };

    current_reboot_reason().inspect_err(|e| warn!("{e:#}")).ok()
}

#[cfg(feature = "mock")]
pub fn write_reboot_reason(_reason: &str, _extra_info: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn current_reboot_reason_ok() {
        std::env::set_var("REBOOT_REASON_DIR_PATH", "testfiles/positive/reboot_reason");
        assert_eq!(
            current_reboot_reason(),
            Some(json!( {
                "datetime": "".to_string(),
                "timeepoch": "".to_string(),
                "uptime": "".to_string(),
                "boot_id": "".to_string(),
                "os_version": "".to_string(),
                "reason": "power-loss".to_string(),
                "extra_info": "".to_string()
            }))
        );
    }
}
