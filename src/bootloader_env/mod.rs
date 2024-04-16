#[cfg(feature = "bootloader_grub")]
mod grub_env;
#[cfg(feature = "bootloader_uboot")]
mod uboot_env;

#[cfg(feature = "bootloader_grub")]
use grub_env::{
    bootloader_env as get, set_bootloader_env as set, unset_bootloader_env as unset,
};
#[cfg(feature = "bootloader_uboot")]
use uboot_env::{
    bootloader_env as get, set_bootloader_env as set, unset_bootloader_env as unset,
};
use anyhow::Result;

#[allow(unreachable_code, unused_variables)]
pub fn bootloader_env(key: &str) -> Result<String> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return get(key);

    Ok("".to_string())
}

#[allow(unreachable_code, unused_variables)]
pub fn set_bootloader_env(key: &str, value: &str) -> Result<()> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return set(key, value);

    Ok(())
}

#[allow(unreachable_code, unused_variables)]
pub fn unset_bootloader_env(key: &str) -> Result<()> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return unset(key);

    Ok(())
}
