#[cfg(feature = "bootloader_grub")]
mod grub;
#[cfg(feature = "bootloader_uboot")]
mod uboot;

#[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
static SUDO_BIN: &str = "/usr/bin/sudo";

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

#[cfg(not(any(feature = "bootloader_grub", feature = "bootloader_uboot")))]
mod mock_store {
    use std::{collections::BTreeMap, sync::Mutex};

    pub static STORE: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

    pub fn store() -> std::sync::MutexGuard<'static, BTreeMap<String, String>> {
        STORE.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[allow(unreachable_code, unused_variables)]
pub fn get(key: &str) -> Result<String> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return get_inner(key);

    #[cfg(not(any(feature = "bootloader_grub", feature = "bootloader_uboot")))]
    {
        let guard = mock_store::store();
        Ok(guard.get(key).cloned().unwrap_or_default())
    }
}

#[allow(unreachable_code, unused_variables)]
pub fn set(key: &str, value: &str) -> Result<()> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return set_inner(key, value);

    #[cfg(not(any(feature = "bootloader_grub", feature = "bootloader_uboot")))]
    {
        let mut guard = mock_store::store();
        guard.insert(key.to_string(), value.to_string());
        Ok(())
    }
}

#[allow(unreachable_code, unused_variables)]
pub fn unset(key: &str) -> Result<()> {
    #[cfg(any(feature = "bootloader_grub", feature = "bootloader_uboot"))]
    return unset_inner(key);

    #[cfg(not(any(feature = "bootloader_grub", feature = "bootloader_uboot")))]
    {
        let mut guard = mock_store::store();
        guard.remove(key);
        Ok(())
    }
}

/// Clears the mock store. Call this at the start of each test that uses
/// bootloader_env, to prevent state leaking between tests.
#[cfg(all(
    not(any(feature = "bootloader_grub", feature = "bootloader_uboot")),
    test
))]
pub(crate) fn clear_mock() {
    mock_store::store().clear();
}

/// Shared lock for all tests that mutate the global mock store or
/// process-wide env vars used by bootloader_env.  Both
/// `firmware_update::tests` and `update_validation::tests` must acquire
/// this to prevent concurrent mutation.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
