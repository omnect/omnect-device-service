// reboot reason handling

use anyhow::{ensure, Context, Result};
use std::process::Command;

static REBOOT_REASON_SCRIPT: &str = "/usr/sbin/omnect_reboot_reason.sh";

pub fn reboot_reason(reason: &str, extra_info: &str) -> Result<()> {
    // make arguments shell script proof
    let cmd = "log";
    let reason = format!("{reason_str}", reason_str = reason.replace("\"", "'"));
    let extra_info = format!("{extra_info_str}", extra_info_str = extra_info.replace("\"", "'"));
    ensure!(
        Command::new("sudo")
            .args([REBOOT_REASON_SCRIPT, cmd, &reason.clone(), &extra_info.clone()])
            .status()
            .context("failed to invoke '{REBOOT_REASON_SCRIPT} {reason} \"{extra_info}\"'")?
            .success(),
        "'{REBOOT_REASON_SCRIPT} {reason} \"{extra_info}\"' failed"
    );

    Ok(())
}
