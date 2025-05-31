use crate::common::from_json_file;
use anyhow::{Context, Result};
use log::warn;
use regex_lite::Regex;

#[cfg(not(feature = "mock"))]
static REBOOT_REASON_SCRIPT: &str = "/usr/sbin/omnect_reboot_reason.sh";
static REBOOT_REASON_DIR_REGEX: &str = r"^\d{6}\+\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2}$";
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
    let current_reboot_reason_impl = || -> Result<serde_json::Value> {
        let regex = Regex::new(REBOOT_REASON_DIR_REGEX)
            .context("failed to create regex for reboot reason folder")?;
        let dir = std::fs::read_dir(reboot_reason_dir_path!())
            .context("failed to read reboot reason directory")?;

        // 1. filter for all dirs with format of REBOOT_REASON_DIR_REGEX, e.g. "000001+2025-04-03_17-42-53"
        // 2. return the latest one
        let dir = dir
            .flatten()
            .filter(|f| {
                let Ok(m) = f.metadata() else { return false };

                if !m.is_dir() {
                    return false;
                }

                let name = f.file_name();
                let Some(name) = name.as_os_str().to_str() else {
                    return false;
                };

                regex.is_match(name)
            })
            .max_by_key(|k| k.file_name())
            .context("failed to identify current reboot reason folder")?;

        let json: serde_json::Value = from_json_file(dir.path().join(REBOOT_REASON_FILE_NAME))?;
        let reason = &json["reboot_reason"]["reason"]
            .as_str()
            .context("failed to get reason")?;
        let current_boot_id = &json["report"]["boot_id"]
            .as_str()
            .context("failed to get boot_id")?;

        Ok(serde_json::json!({
            "current_boot_id": current_boot_id,
            "reason": reason
        }))
    };

    current_reboot_reason_impl()
        .inspect_err(|e| warn!("{e:#}"))
        .ok()
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
        crate::common::set_env_var("REBOOT_REASON_DIR_PATH", "testfiles/positive/reboot_reason");
        assert_eq!(
            current_reboot_reason(),
            Some(json!( {
                "current_boot_id": "56e51a56-f85f-4abd-95bb-fc2335d9b696",
                "reason": "power-loss"
            }))
        );
    }
}
