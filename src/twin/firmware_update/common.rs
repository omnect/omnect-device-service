use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
pub struct UpdateValidationConfig {
    pub local: bool,
}

pub static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";
pub static IOT_HUB_DEVICE_UPDATE_SERVICE_TIMER: &str = "deviceupdate-agent.timer";

pub static OMNECT_EXTRA_BOOTARGS: &str = "omnect_extra_bootargs";
pub static OMNECT_VALIDATE_EXTRA_BOOTARGS: &str = "omnect_validate_extra_bootargs";
pub static OMNECT_VALIDATE_UPDATE: &str = "omnect_validate_update";
pub static OMNECT_VALIDATE_UPDATE_PART: &str = "omnect_validate_update_part";
pub static OMNECT_BOOTLOADER_UPDATED: &str = "omnect_bootloader_updated";
pub static OMNECT_OS_BOOTPART: &str = "omnect_os_bootpart";

/// Sentinel value written to `omnect_validate_extra_bootargs` to signal
/// that the extra bootargs should be *removed* during update validation.
pub static NOARGS_SENTINEL: &str = "#noargs";

macro_rules! update_validation_config_path {
    () => {
        env::var("UPDATE_VALIDATION_CONFIG_PATH")
            .unwrap_or("/var/lib/omnect-device-service/update_validation_conf.json".to_string())
    };
}

macro_rules! update_folder_path {
    () => {
        env::var("UPDATE_FOLDER_PATH")
            .unwrap_or("/var/lib/omnect-device-service/local_update".to_string())
    };
}

macro_rules! du_config_path {
    () => {
        env::var("DEVICE_UPDATE_PATH").unwrap_or("/etc/adu/du-config.json".to_string())
    };
}

macro_rules! log_file_path {
    () => {
        env::var("SWUPDATE_LOG_PATH").unwrap_or("/var/log/aduc-logs/swupdate.log".to_string())
    };
}

macro_rules! pubkey_file_path {
    () => {
        env::var("SWUPDATE_PUBKEY_PATH").unwrap_or("/usr/share/swupdate/public.pem".to_string())
    };
}

macro_rules! no_bootloader_updated_file_path {
    () => {
        env::var("NO_BOOTLOADER_UPDATE_PATH")
            .unwrap_or("/tmp/omnect-bootloader-update-not-necessary".to_string())
    };
}

macro_rules! bootloader_updated_file_path {
    () => {
        env::var("BOOTLOADER_UPDATE_PATH").unwrap_or("/tmp/omnect-bootloader-update".to_string())
    };
}

macro_rules! bootargs_omnect_file_path {
    () => {
        env::var("BOOTARGS_OMNECT_FILE_PATH")
            .unwrap_or("/boot/omnect_extra_bootargs_omnect".to_string())
    };
}

macro_rules! bootargs_custom_file_path {
    () => {
        env::var("BOOTARGS_CUSTOM_FILE_PATH")
            .unwrap_or("/boot/omnect_extra_bootargs_custom".to_string())
    };
}

macro_rules! bootargs_omnect_backup_file_path {
    () => {
        env::var("BOOTARGS_OMNECT_BACKUP_FILE_PATH")
            .unwrap_or("/tmp/omnect_extra_bootargs_omnect.backup".to_string())
    };
}

pub(crate) use bootargs_custom_file_path;
pub(crate) use bootargs_omnect_backup_file_path;
pub(crate) use bootargs_omnect_file_path;
pub(crate) use bootloader_updated_file_path;
pub(crate) use du_config_path;
pub(crate) use log_file_path;
pub(crate) use no_bootloader_updated_file_path;
pub(crate) use pubkey_file_path;
pub(crate) use update_folder_path;
pub(crate) use update_validation_config_path;
