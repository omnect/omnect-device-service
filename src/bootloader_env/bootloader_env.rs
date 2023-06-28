#[cfg(all(feature = "bootloader_grub", not(any(test, feature = "mock"))))]
use super::grub_env;
#[cfg(all(feature = "bootloader_uboot", not(any(test, feature = "mock"))))]
use super::uboot_env;
use anyhow::Result;

#[cfg(not(any(test, feature = "mock")))]
pub fn bootloader_env(key: &str) -> Result<String> {
    #[cfg(feature = "bootloader_grub")]
    return grub_env::bootloader_env(key);

    #[cfg(feature = "bootloader_uboot")]
    return uboot_env::bootloader_env(key);
}

#[cfg(not(any(test, feature = "mock")))]
pub fn set_bootloader_env(key: &str, value: &str) -> Result<()> {
    #[cfg(feature = "bootloader_grub")]
    return grub_env::set_bootloader_env(key, value);

    #[cfg(feature = "bootloader_uboot")]
    return uboot_env::set_bootloader_env(key, value);
}

#[cfg(not(any(test, feature = "mock")))]
pub fn unset_bootloader_env(key: &str) -> Result<()> {
    #[cfg(feature = "bootloader_grub")]
    return grub_env::unset_bootloader_env(key);

    #[cfg(feature = "bootloader_uboot")]
    return uboot_env::unset_bootloader_env(key);
}

#[cfg(any(test, feature = "mock"))]
pub fn bootloader_env(_key: &str) -> Result<String> {
    Ok("".to_string())
}

#[cfg(any(test, feature = "mock"))]
pub fn set_bootloader_env(_key: &str, _value: &str) -> Result<()> {
    Ok(())
}

#[cfg(any(test, feature = "mock"))]
pub fn unset_bootloader_env(_key: &str) -> Result<()> {
    Ok(())
}
