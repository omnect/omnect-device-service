// reboot reason handling

use anyhow::Result;

// NOTE:
//   repetetive use of cfg macro is ugly, yes, but having a separate reboot
//   reason file for mocking purposes is also not ideal
#[cfg(not(feature = "mock"))]
use {
    anyhow::{ensure, Context},
    std::process::Command,
};

#[cfg(not(feature = "mock"))]
static REBOOT_REASON_SCRIPT: &str = "/usr/sbin/omnect_reboot_reason.sh";

#[cfg(not(feature = "mock"))]
pub fn reboot_reason(reason: &str, extra_info: &str) -> Result<()> {
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
    ensure!(
        cmd.args(common_args)
            .status()
            .context("failed to invoke '{REBOOT_REASON_SCRIPT} {reason} \"{extra_info}\"'")?
            .success(),
        "'{REBOOT_REASON_SCRIPT} {reason} \"{extra_info}\"' failed"
    );

    Ok(())
}

#[cfg(feature = "mock")]
pub fn reboot_reason(_reason: &str, _extra_info: &str) -> Result<()> {
    Ok(())
}
