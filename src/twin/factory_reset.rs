use crate::{
    bootloader_env,
    common::from_json_file,
    systemd,
    twin::{Feature, feature::*},
    web_service,
};
use anyhow::{Context, Result, bail};
use azure_iot_sdk::client::IotMessage;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::{from_reader, json};
use serde_repr::*;
use std::{
    collections::HashMap,
    env,
    fs::{File, read_dir},
    io::BufReader,
    path::Path,
};
use tokio::sync::mpsc::Sender;

macro_rules! result_path {
    () => {
        env::var("FACTORY_RESET_RESULT_FILE_PATH")
            .unwrap_or("/run/omnect-device-service/omnect-os-initramfs.json".to_string())
    };
}

macro_rules! config_path {
    () => {
        env::var("FACTORY_RESET_CONFIG_FILE_PATH")
            .unwrap_or("/etc/omnect/factory-reset.json".to_string())
    };
}

macro_rules! custom_config_dir_path {
    () => {
        env::var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH")
            .unwrap_or("/etc/omnect/factory-reset.d".to_string())
    };
}

#[derive(Clone, Debug, Deserialize_repr, PartialEq, Serialize_repr)]
#[repr(u8)]
pub enum FactoryResetMode {
    Mode1 = 1,
    Mode2 = 2,
    Mode3 = 3,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FactoryResetCommand {
    pub mode: FactoryResetMode,
    pub preserve: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CustomConfig {
    // only used to parse
    #[allow(dead_code)]
    paths: Vec<String>,
}

// u32 matches the serialization in omnect-os-init
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(from = "u32", into = "u32")]
pub enum FactoryResetStatus {
    Success,
    Invalid,
    Error,
    ConfigError,
    Warning,
    Unknown(u32),
}

impl From<u32> for FactoryResetStatus {
    fn from(status: u32) -> Self {
        match status {
            0 => Self::Success,
            1 => Self::Invalid,
            2 => Self::Error,
            3 => Self::ConfigError,
            4 => Self::Warning,
            other => Self::Unknown(other),
        }
    }
}

impl From<FactoryResetStatus> for u32 {
    fn from(status: FactoryResetStatus) -> Self {
        match status {
            FactoryResetStatus::Success => 0,
            FactoryResetStatus::Invalid => 1,
            FactoryResetStatus::Error => 2,
            FactoryResetStatus::ConfigError => 3,
            FactoryResetStatus::Warning => 4,
            FactoryResetStatus::Unknown(other) => other,
        }
    }
}

// absent options serialize as null: twin reports are merge patches, so an
// omitted key would keep the value of a previous result.
// data_wiped has no default: a missing key must not read as a safe abort.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct FactoryResetResult {
    status: FactoryResetStatus,
    context: Option<String>,
    error: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    data_wiped: bool,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct FactoryResetReport {
    keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<FactoryResetResult>,
}

#[derive(Debug, PartialEq)]
enum FactoryResetResultState {
    Reported(FactoryResetResult),
    NoReset,
    Unusable,
}

pub struct FactoryReset {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    report: FactoryResetReport,
    result_unusable: bool,
}

impl Feature for FactoryReset {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::FACTORY_RESET_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_FACTORY_RESET") != Ok("true".to_string())
    }

    async fn connect_web_service(&self) -> Result<()> {
        web_service::publish(
            web_service::PublishChannel::FactoryResetV1,
            serde_json::to_value(&self.report)
                .context("connect_web_service: failed to serialize")?,
        )
        .await;

        Ok(())
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        tx_reported_properties
            .send(json!({ "factory_reset": self.twin_report()? }))
            .await
            .context("connect_twin: send")?;

        self.tx_reported_properties = Some(tx_reported_properties);

        Ok(())
    }

    async fn command(&mut self, cmd: &Command) -> CommandResult {
        match cmd {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::DirModified,
                ..
            }) => {
                let keys = FactoryReset::factory_reset_keys()?;

                if keys != self.report.keys {
                    debug!("keys changed: {keys:?}");

                    self.report.keys = keys;

                    if let Some(tx_reported_properties) = &self.tx_reported_properties {
                        tx_reported_properties
                            .send(json!({
                                "factory_reset": {
                                    "keys": self.report.keys
                                }
                            }))
                            .await
                            .context("keys changed but cannot report")?;
                    }

                    web_service::publish(
                        web_service::PublishChannel::FactoryResetV1,
                        serde_json::to_value(&self.report)
                            .context("keys changed but cannot publish")?,
                    )
                    .await;
                }
            }
            Command::FactoryReset(cmd) => {
                self.reset_to_factory_settings(cmd).await?;
            }
            _ => bail!("unexpected command"),
        };

        Ok(None)
    }
}

impl FactoryReset {
    const FACTORY_RESET_VERSION: u8 = 4;
    const ID: &'static str = "factory_reset";

    pub fn new(fs_watcher: &mut FsWatcher) -> Result<Self> {
        fs_watcher.watch_dir_modified::<FactoryReset>(Path::new(&custom_config_dir_path!()))?;

        let state = FactoryReset::factory_reset_result();
        let result_unusable = matches!(state, FactoryResetResultState::Unusable);
        let result = match state {
            FactoryResetResultState::Reported(result) => Some(result),
            _ => None,
        };

        let report = FactoryResetReport {
            keys: FactoryReset::factory_reset_keys()?,
            result,
        };

        Ok(FactoryReset {
            tx_reported_properties: None,
            report,
            result_unusable,
        })
    }

    fn twin_report(&self) -> Result<serde_json::Value> {
        let mut report =
            serde_json::to_value(&self.report).context("twin_report: failed to serialize")?;

        if self.result_unusable {
            // null deletes the property; omitting it would keep an earlier boot's result
            report["result"] = serde_json::Value::Null;
        }

        Ok(report)
    }

    fn factory_reset_keys() -> Result<Vec<String>> {
        let factory_reset_config: HashMap<String, Vec<std::path::PathBuf>> =
            from_json_file(config_path!())?;

        let mut keys: Vec<String> = factory_reset_config.into_keys().collect();
        if 0 < read_dir(custom_config_dir_path!())
            .context("read factory-reset.d")?
            .filter(|entry| {
                let Ok(entry) = entry else {
                    warn!("factory-reset.d: unexpected entry");
                    return false;
                };
                let file_path = entry.path();
                let Ok(reader) = File::open(&file_path) else {
                    warn!("factory-reset.d: cannot open custom config file '{file_path:?}'");
                    return false;
                };
                let Ok(_): Result<CustomConfig, _> = from_reader(BufReader::new(reader)) else {
                    warn!("factory-reset.d: cannot parse custom config file '{file_path:?}'");
                    return false;
                };

                true
            })
            .count()
        {
            keys.push(String::from("applications"))
        }

        #[cfg(feature = "mock")]
        keys.sort_by_key(|a| a.to_lowercase());

        Ok(keys)
    }

    fn factory_reset_result() -> FactoryResetResultState {
        Self::factory_reset_result_from(&result_path!())
    }

    fn factory_reset_result_from(path: &str) -> FactoryResetResultState {
        let omnect_os_initramfs_json: serde_json::Map<String, serde_json::Value> =
            match from_json_file(path) {
                Ok(json) => json,
                Err(e) => {
                    error!("factory reset: cannot read result: {e:#}");
                    return FactoryResetResultState::Unusable;
                }
            };

        let Some(raw) = omnect_os_initramfs_json
            .get("factory_reset")
            .filter(|raw| !raw.is_null())
        else {
            if omnect_os_initramfs_json
                .get("factory-reset")
                .is_some_and(|legacy| !legacy.is_null())
            {
                error!("factory reset: cannot parse result with unsupported key 'factory-reset'");
                return FactoryResetResultState::Unusable;
            }
            debug!("factory reset: no result");
            return FactoryResetResultState::NoReset;
        };

        let result = match FactoryResetResult::deserialize(raw) {
            Ok(result) => result,
            Err(e) => {
                error!("factory reset: cannot parse result from '{path}': {e:#}");
                return FactoryResetResultState::Unusable;
            }
        };

        if matches!(result.status, FactoryResetStatus::Unknown(_)) {
            warn!("factory reset result with unknown status: {raw:#}");
        } else {
            info!("factory reset result: {result:#?}");
        }

        FactoryResetResultState::Reported(result)
    }

    async fn reset_to_factory_settings(&self, cmd: &FactoryResetCommand) -> CommandResult {
        info!("factory reset requested: {cmd:?}");

        let keys = FactoryReset::factory_reset_keys()?;
        for topic in &cmd.preserve {
            let topic = String::from(topic.to_string().trim_matches('"'));
            if !keys.contains(&topic) {
                anyhow::bail!("unknown preserve topic received: {topic}");
            }
        }

        bootloader_env::set("factory-reset", &serde_json::to_string(&cmd)?)?;
        systemd::reboot("factory-reset", "initiated by portal or API").await?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // the returned dir holds the config files, so it must live as long as the test
    fn new_factory_reset(
        result_path: impl AsRef<std::ffi::OsStr>,
    ) -> (FactoryReset, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let config_file_path = temp_dir.path().join("factory-reset.json");
        let custom_dir_path = temp_dir.path().join("factory-reset.d");

        std::fs::copy("testfiles/positive/factory-reset.json", &config_file_path)
            .expect("copy config");
        std::fs::create_dir_all(&custom_dir_path).expect("create custom dir");

        crate::common::set_env_var("FACTORY_RESET_RESULT_FILE_PATH", result_path);
        crate::common::set_env_var("FACTORY_RESET_CONFIG_FILE_PATH", config_file_path);
        crate::common::set_env_var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH", custom_dir_path);

        let mut fs_watcher = FsWatcher::new().expect("FsWatcher::new");
        let factory_reset = FactoryReset::new(&mut fs_watcher).expect("FactoryReset::new");

        (factory_reset, temp_dir)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn factory_reset_test() {
        let (mut factory_reset, _temp_dir) =
            new_factory_reset("testfiles/positive/omnect-os-initramfs-factory-reset.json");

        assert!(
            factory_reset
                .command(&Command::FactoryReset(FactoryResetCommand {
                    mode: FactoryResetMode::Mode1,
                    preserve: vec!["foo".to_string()],
                }))
                .await
                .expect_err("unknown preserve topic must fail")
                .chain()
                .any(|e| e
                    .to_string()
                    .starts_with("unknown preserve topic received: foo"))
        );

        factory_reset
            .command(&Command::FactoryReset(FactoryResetCommand {
                mode: FactoryResetMode::Mode1,
                preserve: vec![
                    "network".to_string(),
                    "firewall".to_string(),
                    "certificates".to_string(),
                ],
            }))
            .await
            .expect("factory reset command");

        let (tx_reported_properties, mut rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let (tx_outgoing_message, mut _rx_outgoing_message) = tokio::sync::mpsc::channel(100);

        factory_reset
            .connect_twin(tx_reported_properties, tx_outgoing_message)
            .await
            .expect("connect_twin");

        let reported: FactoryResetResult = serde_json::from_value(
            rx_reported_properties
                .recv()
                .await
                .expect("reported properties")["factory_reset"]["result"]
                .clone(),
        )
        .expect("parse reported result");

        assert_eq!(
            reported,
            FactoryResetResult {
                status: FactoryResetStatus::Success,
                error: None,
                context: None,
                paths: vec!["/etc/omnect/factory-reset.d/".to_string()],
                data_wiped: true,
            }
        );
    }

    #[test]
    fn factory_reset_keys_test() {
        crate::common::set_env_var(
            "FACTORY_RESET_CONFIG_FILE_PATH",
            "testfiles/positive/factory-rest.json",
        );
        assert!(
            FactoryReset::factory_reset_keys()
                .expect_err("missing config must fail")
                .to_string()
                .starts_with("failed to open for read: ")
        );

        let tmp_dir = tempfile::tempdir().expect("create temp dir");
        let file_path = tmp_dir.path().join("factory-reset.json");
        crate::common::set_env_var("FACTORY_RESET_CONFIG_FILE_PATH", file_path.clone());

        std::fs::copy(
            "testfiles/positive/factory-reset.json",
            file_path.clone().as_path(),
        )
        .expect("copy config");

        assert!(
            FactoryReset::factory_reset_keys()
                .expect_err("missing custom config dir must fail")
                .to_string()
                .starts_with("read factory-reset.d")
        );

        let custom_dir_path = tmp_dir.path().join("factory-reset.d");
        std::fs::create_dir(custom_dir_path.clone()).expect("create custom dir");
        crate::common::set_env_var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH", custom_dir_path);

        FactoryReset::factory_reset_keys().expect("factory_reset_keys");
    }

    #[test]
    fn factory_reset_status_test() {
        for file in [
            "",
            "testfiles/negative/omnect-os-initramfs-truncated.json",
            "testfiles/negative/omnect-os-initramfs-array.json",
            "testfiles/negative/omnect-os-initramfs-string.json",
            "testfiles/negative/omnect-os-initramfs-null.json",
            "testfiles/negative/omnect-os-initramfs-factory-reset-format.json",
            "testfiles/negative/omnect-os-initramfs-factory-reset-no-status.json",
            "testfiles/negative/omnect-os-initramfs-factory-reset-negative-status.json",
            "testfiles/negative/omnect-os-initramfs-factory-reset-status-out-of-range.json",
            "testfiles/negative/omnect-os-initramfs-factory-reset-no-data-wiped.json",
            "testfiles/negative/omnect-os-initramfs-factory-reset-legacy-key.json",
        ] {
            assert_eq!(
                FactoryReset::factory_reset_result_from(file),
                FactoryResetResultState::Unusable,
                "{file}"
            );
        }

        for file in [
            "testfiles/positive/omnect-os-initramfs-normal-boot.json",
            "testfiles/positive/omnect-os-initramfs-factory-reset-null.json",
            "testfiles/positive/omnect-os-initramfs-factory-reset-legacy-key-null.json",
        ] {
            assert_eq!(
                FactoryReset::factory_reset_result_from(file),
                FactoryResetResultState::NoReset,
                "{file}"
            );
        }

        assert_eq!(
            FactoryReset::factory_reset_result_from(
                "testfiles/positive/omnect-os-initramfs-factory-reset.json"
            ),
            FactoryResetResultState::Reported(FactoryResetResult {
                status: FactoryResetStatus::Success,
                error: None,
                paths: vec!["/etc/omnect/factory-reset.d/".to_string()],
                context: None,
                data_wiped: true,
            })
        );

        assert_eq!(
            FactoryReset::factory_reset_result_from(
                "testfiles/positive/omnect-os-initramfs-factory-reset-warning.json"
            ),
            FactoryResetResultState::Reported(FactoryResetResult {
                status: FactoryResetStatus::Warning,
                error: None,
                paths: vec![
                    "/etc/omnect/factory-reset.d/".to_string(),
                    "/etc/NetworkManager/system-connections/".to_string(),
                    "/etc/systemd/network/".to_string(),
                    "/etc/wpa_supplicant.conf".to_string(),
                ],
                context: Some("data reformatted twice".to_string()),
                data_wiped: true,
            })
        );

        assert_eq!(
            FactoryReset::factory_reset_result_from(
                "testfiles/positive/omnect-os-initramfs-factory-reset-unknown-status.json"
            ),
            FactoryResetResultState::Reported(FactoryResetResult {
                status: FactoryResetStatus::Unknown(300),
                error: Some("error of a future status code".to_string()),
                paths: vec![],
                context: None,
                data_wiped: false,
            })
        );
    }

    // the files in testfiles/positive/contract are copies of the factory reset
    // results that omnect-os-init writes; keep them in sync with its tests/contract
    #[test]
    fn factory_reset_contract_files_test() {
        let result =
            |status, error: Option<&str>, context: Option<&str>, paths: &[&str], data_wiped| {
                FactoryResetResult {
                    status,
                    error: error.map(str::to_string),
                    context: context.map(str::to_string),
                    paths: paths.iter().map(|p| p.to_string()).collect(),
                    data_wiped,
                }
            };

        for (name, expected) in [
            (
                "success",
                result(
                    FactoryResetStatus::Success,
                    None,
                    None,
                    &["/etc/hostname"],
                    true,
                ),
            ),
            (
                "invalid",
                result(
                    FactoryResetStatus::Invalid,
                    Some("unknown factory reset mode 7"),
                    None,
                    &[],
                    false,
                ),
            ),
            (
                "error-before-wipe",
                result(
                    FactoryResetStatus::Error,
                    Some("backup of /etc/hostname failed"),
                    None,
                    &[],
                    false,
                ),
            ),
            (
                "error-after-wipe",
                result(
                    FactoryResetStatus::Error,
                    Some("backup missing for /etc/hostname"),
                    Some("etc/hostname:missing-backup"),
                    &["/etc/hostname"],
                    true,
                ),
            ),
            (
                "config-error",
                result(
                    FactoryResetStatus::ConfigError,
                    Some("unknown preserve topic foo"),
                    None,
                    &[],
                    false,
                ),
            ),
            (
                "warning",
                result(
                    FactoryResetStatus::Warning,
                    None,
                    Some("data reformatted twice"),
                    &["/etc/hostname"],
                    true,
                ),
            ),
        ] {
            let file = format!("testfiles/positive/contract/status-factory-reset-{name}.json");
            assert_eq!(
                FactoryReset::factory_reset_result_from(&file),
                FactoryResetResultState::Reported(expected),
                "{file}"
            );
        }
    }

    #[test]
    fn twin_report_test() {
        let report = || FactoryResetReport {
            keys: vec![],
            result: None,
        };

        let unusable = FactoryReset {
            tx_reported_properties: None,
            report: report(),
            result_unusable: true,
        };
        assert_eq!(
            unusable.twin_report().expect("twin_report")["result"],
            serde_json::Value::Null
        );

        let no_reset = FactoryReset {
            tx_reported_properties: None,
            report: report(),
            result_unusable: false,
        };
        assert!(
            no_reset
                .twin_report()
                .expect("twin_report")
                .get("result")
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_without_result_file_test() {
        let (factory_reset, _temp_dir) = new_factory_reset("testfiles/does-not-exist.json");

        assert!(factory_reset.report.result.is_none());
        assert!(factory_reset.result_unusable);
    }

    #[test]
    fn factory_reset_new_with_unknown_status_test() {
        let (factory_reset, _temp_dir) = new_factory_reset(
            "testfiles/positive/omnect-os-initramfs-factory-reset-unknown-status.json",
        );

        assert!(matches!(
            factory_reset.report.result,
            Some(FactoryResetResult {
                status: FactoryResetStatus::Unknown(300),
                ..
            })
        ));
        assert!(!factory_reset.result_unusable);
        assert_eq!(
            factory_reset.twin_report().expect("twin_report")["result"]["status"],
            json!(300)
        );
    }

    #[test]
    fn factory_reset_new_without_data_wiped_test() {
        let (factory_reset, _temp_dir) = new_factory_reset(
            "testfiles/negative/omnect-os-initramfs-factory-reset-no-data-wiped.json",
        );

        assert!(factory_reset.result_unusable);
        assert_eq!(
            factory_reset.twin_report().expect("twin_report")["result"],
            serde_json::Value::Null
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dir_modified_command_test() {
        let (mut factory_reset, _temp_dir) =
            new_factory_reset("testfiles/positive/omnect-os-initramfs-factory-reset.json");

        let result = factory_reset
            .command(&Command::FsEvent(FsEventCommand {
                kind: FsEventKind::DirModified,
                feature_id: std::any::TypeId::of::<FactoryReset>(),
                path: std::path::PathBuf::from("/unused"),
            }))
            .await;
        assert!(result.is_ok());
    }
}
