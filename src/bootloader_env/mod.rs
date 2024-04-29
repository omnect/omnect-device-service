#[cfg(feature = "bootloader_grub")]
mod grub;
#[cfg(feature = "bootloader_uboot")]
mod uboot;

use anyhow::Result;
#[cfg(feature = "bootloader_grub")]
use grub::{
    bootloader_env as get_inner, set_bootloader_env as set_inner,
    unset_bootloader_env as unset_inner,
};
#[cfg(feature = "bootloader_uboot")]
use uboot::{
    bootloader_env as get_inner, set_bootloader_env as set_inner,
    unset_bootloader_env as unset_inner,
};

#[allow(unreachable_code, unused_variables)]
pub fn get(key: &str) -> Result<String> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return get_inner(key);

    Ok("".to_string())
}

#[allow(unreachable_code, unused_variables)]
pub fn set(key: &str, value: &str) -> Result<()> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return set_inner(key, value);

    Ok(())
}

#[allow(unreachable_code, unused_variables)]
pub fn unset(key: &str) -> Result<()> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return unset_inner(key);

    Ok(())
}
