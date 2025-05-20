use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
pub struct UpdateValidationConfig {
    pub local: bool,
}

pub static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";
pub static IOT_HUB_DEVICE_UPDATE_SERVICE_TIMER: &str = "deviceupdate-agent.timer";

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

#[cfg(not(feature = "mock"))]
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

pub(crate) use bootloader_updated_file_path;
pub(crate) use du_config_path;
pub(crate) use log_file_path;
pub(crate) use no_bootloader_updated_file_path;
#[cfg(not(feature = "mock"))]
pub(crate) use pubkey_file_path;
pub(crate) use update_folder_path;
pub(crate) use update_validation_config_path;
