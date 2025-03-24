// reboot reason handling

use anyhow::{ensure, Context, Result};
use std::process::Command;

static REBOOT_REASON_SCRIPT: &str = "/usr/sbin/omnect_reboot_reason.sh";

pub fn reboot_reason(reason: &str, extra_info: &str) -> Result<()> {
    // make arguments shell script proof
    let reboot_reason_cmd = "log";
    let reason = format!("{reason_str}", reason_str = reason.replace("\"", "'"));
    let extra_info = format!("{extra_info_str}", extra_info_str = extra_info.replace("\"", "'"));

    let common_args = [reboot_reason_cmd, &reason, &extra_info];
    let mut cmd: Command;
    // we need to pass sudo only for EFI machines which correlates to feature
    // bootloader_grub
    if cfg!(feature = "bootloader_grub") {
	cmd = Command::new("sudo");
	cmd.args([ REBOOT_REASON_SCRIPT ]);
    } else if cfg!(feature = "bootloader_uboot") {
	cmd = Command::new(REBOOT_REASON_SCRIPT);
    } else if cfg!(feature = "mock") {
	return Ok(());
    } else {
	unreachable!()
    };
    ensure!(
        cmd.args(common_args)
            .status()
            .context("failed to invoke '{REBOOT_REASON_SCRIPT} {reason} \"{extra_info}\"'")?
            .success(),
        "'{REBOOT_REASON_SCRIPT} {reason} \"{extra_info}\"' failed"
    );

    Ok(())
}
