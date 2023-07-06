#[allow(clippy::module_inception)]
pub mod bootloader_env;
#[cfg(all(feature = "bootloader_grub", not(test)))]
mod grub_env;
#[cfg(all(feature = "bootloader_uboot", not(test)))]
mod uboot_env;
