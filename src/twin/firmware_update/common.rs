use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Default, Deserialize, Serialize)]
pub struct UpdateValidationConfig {
    pub local: bool,
}

pub static IOT_HUB_DEVICE_UPDATE_SERVICE: &str = "deviceupdate-agent.service";

macro_rules! update_validation_config_path {
    () => {{
        static UPDATE_VALIDATION_CONFIG_PATH_DEFAULT: &'static str =
            "/var/lib/omnect-device-service/update_validation_conf.json";
        std::env::var("UPDATE_VALIDATION_CONFIG_PATH")
            .unwrap_or(UPDATE_VALIDATION_CONFIG_PATH_DEFAULT.to_string())
    }};
}

macro_rules! update_folder_path {
    () => {{
        static UPDATE_FOLDER_PATH_DEFAULT: &'static str =
            "/var/lib/omnect-device-service/local_update";
        std::env::var("UPDATE_FOLDER_PATH").unwrap_or(UPDATE_FOLDER_PATH_DEFAULT.to_string())
    }};
}

macro_rules! du_config_path {
    () => {{
        static DEVICE_UPDATE_PATH_DEFAULT: &'static str = "/etc/adu/du-config.json";
        std::env::var("DEVICE_UPDATE_PATH").unwrap_or(DEVICE_UPDATE_PATH_DEFAULT.to_string())
    }};
}

macro_rules! log_file_path {
    () => {{
        static SWUPDATE_LOG_PATH_DEFAULT: &'static str = "/var/log/aduc-logs/swupdate.log";
        std::env::var("SWUPDATE_LOG_PATH").unwrap_or(SWUPDATE_LOG_PATH_DEFAULT.to_string())
    }};
}

#[cfg(not(feature = "mock"))]
macro_rules! pubkey_file_path {
    () => {{
        static SWUPDATE_PUBKEY_PATH_DEFAULT: &'static str = "/usr/share/swupdate/public.pem";
        std::env::var("SWUPDATE_PUBKEY_PATH").unwrap_or(SWUPDATE_PUBKEY_PATH_DEFAULT.to_string())
    }};
}

macro_rules! no_bootloader_updated_file_path {
    () => {{
        static NO_BOOTLOADER_UPDATE_PATH_DEFAULT: &'static str =
            "/run/omnect-bootloader-update-not-necessary";
        std::env::var("NO_BOOTLOADER_UPDATE_PATH")
            .unwrap_or(NO_BOOTLOADER_UPDATE_PATH_DEFAULT.to_string())
    }};
}

macro_rules! bootloader_updated_file_path {
    () => {{
        static BOOTLOADER_UPDATE_PATH_DEFAULT: &'static str = "/run/omnect-bootloader-update";
        std::env::var("BOOTLOADER_UPDATE_PATH")
            .unwrap_or(BOOTLOADER_UPDATE_PATH_DEFAULT.to_string())
    }};
}

pub(super) use bootloader_updated_file_path;
pub(super) use du_config_path;
pub(super) use log_file_path;
pub(super) use no_bootloader_updated_file_path;
#[cfg(not(feature = "mock"))]
pub(super) use pubkey_file_path;
pub(super) use update_folder_path;
pub(super) use update_validation_config_path;

pub fn json_to_file<T, P>(value: &T, path: P, create: bool) -> Result<()>
where
    T: ?Sized + Serialize,
    P: AsRef<std::path::Path>,
    P: std::fmt::Display,
{
    serde_json::to_writer_pretty(
        std::fs::OpenOptions::new()
            .write(true)
            .create(create)
            .truncate(true)
            .open(&path)
            .context(format!("failed to open for write: {path}"))?,
        value,
    )
    .context(format!("failed to write to: {path}"))
}

pub fn json_from_file<P, T>(path: P) -> Result<T>
where
    P: AsRef<std::path::Path>,
    P: std::fmt::Display,
    T: DeserializeOwned,
{
    serde_json::from_reader(
        std::fs::OpenOptions::new()
            .read(true)
            .create(false)
            .open(&path)
            .context(format!("failed to open for read: {path}"))?,
    )
    .context(format!("failed to read from: {path}"))
}
