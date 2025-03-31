use crate::{
    bootloader_env,
    common::from_json_file,
    reboot_reason, systemd,
    twin::{feature::*, Feature},
    web_service,
};
use anyhow::{bail, Context, Result};
use azure_iot_sdk::client::IotMessage;
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_repr::*;
use std::{collections::HashMap, env, fs::read_dir};
use tokio::sync::mpsc::Sender;

macro_rules! factory_reset_status_path {
    () => {{
        static FACTORY_RESET_STATUS_FILE_PATH_DEFAULT: &'static str =
            "/run/omnect-device-service/omnect-os-initramfs.json";
        std::env::var("FACTORY_RESET_STATUS_FILE_PATH")
            .unwrap_or(FACTORY_RESET_STATUS_FILE_PATH_DEFAULT.to_string())
    }};
}

macro_rules! factory_reset_config_path {
    () => {{
        static FACTORY_RESET_CONFIG_FILE_PATH_DEFAULT: &'static str =
            "/etc/omnect/factory-reset.json";
        std::env::var("FACTORY_RESET_CONFIG_FILE_PATH")
            .unwrap_or(FACTORY_RESET_CONFIG_FILE_PATH_DEFAULT.to_string())
    }};
}

macro_rules! factory_reset_custom_config_dir_path {
    () => {{
        static FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH_DEFAULT: &'static str =
            "/etc/omnect/factory-reset.d";
        std::env::var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH")
            .unwrap_or(FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH_DEFAULT.to_string())
    }};
}

#[derive(Clone, Debug, Deserialize_repr, PartialEq, Serialize_repr)]
#[repr(u8)]
pub enum FactoryResetMode {
    Mode1 = 1,
    Mode2 = 2,
    Mode3 = 3,
    Mode4 = 4,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FactoryResetCommand {
    pub mode: FactoryResetMode,
    pub preserve: Vec<String>,
}

#[derive(Debug, Deserialize_repr, PartialEq, Serialize_repr)]
#[repr(u8)]
pub enum FactoryResetStatus {
    ModeSupported = 0,
    ModeUnsupported = 1,
    BackupRestoreError = 2,
    ConfigurationError = 3,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct FactoryResetResult {
    status: FactoryResetStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<String>,
    error: String,
    paths: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct FactoryResetReport {
    keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<FactoryResetResult>,
}

pub struct FactoryReset {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    report: FactoryResetReport,
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
            .send(json!({
                "factory_reset": &self.report
            }))
            .await
            .context("report_factory_reset_status: send")?;

        self.tx_reported_properties = Some(tx_reported_properties);

        Ok(())
    }

    async fn command(&mut self, cmd: &Command) -> CommandResult {
        info!("factory reset requested: {cmd:?}");

        let Command::FactoryReset(cmd) = cmd else {
            bail!("unexpected command")
        };

        self.reset_to_factory_settings(cmd).await?;

        Ok(None)
    }
}

impl FactoryReset {
    const FACTORY_RESET_VERSION: u8 = 3;
    const ID: &'static str = "factory_reset";

    pub fn new() -> Result<Self> {
        let report = FactoryResetReport {
            keys: FactoryReset::factory_reset_keys()?,
            result: FactoryReset::factory_reset_result()?,
        };

        Ok(FactoryReset {
            tx_reported_properties: None,
            report,
        })
    }

    fn factory_reset_keys() -> Result<Vec<String>> {
        let factory_reset_config: HashMap<String, Vec<std::path::PathBuf>> =
            from_json_file(factory_reset_config_path!())?;

        let mut keys: Vec<String> = factory_reset_config.into_keys().collect();
        if read_dir(factory_reset_custom_config_dir_path!())
            .context("read factory-reset.d")?
            .next()
            .is_some()
        {
            keys.push(String::from("applications"))
        }

        #[cfg(feature = "mock")]
        keys.sort_by_key(|a| a.to_lowercase());

        Ok(keys)
    }

    fn factory_reset_result() -> Result<Option<FactoryResetResult>> {
        let omnect_os_initramfs_json: serde_json::Value =
            from_json_file(&factory_reset_status_path!())?;

        if omnect_os_initramfs_json["factory-reset"].is_null() {
            debug!("factory reset: no result");
            return Ok(None);
        }

        let result = serde_json::from_value(omnect_os_initramfs_json["factory-reset"].clone())
            .context("failed to parse factory reset result from initramfs")?;

        info!("factory reset result: {result:#?}");

        Ok(Some(result))
    }

    async fn reset_to_factory_settings(&self, cmd: &FactoryResetCommand) -> CommandResult {
        let keys = FactoryReset::factory_reset_keys()?;
        for topic in &cmd.preserve {
            let topic = String::from(topic.to_string().trim_matches('"'));
            if !keys.contains(&topic) {
                anyhow::bail!("unknown preserve topic received: {topic}");
            }
        }

        bootloader_env::set("factory-reset", &serde_json::to_string(&cmd)?)?;

        if let Err(e) =
            reboot_reason::write_reboot_reason("factory-reset", "initiated by portal or API")
        {
            error!("reset_to_factory_settings: failed to write reboot reason with {e:#}");
        }
        systemd::reboot().await?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn factory_reset_test() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_file_path = temp_dir.path().join("factory-reset.json");
        let custom_dir_path = temp_dir.path().join("factory-reset.d");

        std::fs::copy(
            "testfiles/positive/factory-reset.json",
            config_file_path.clone().as_path(),
        )
        .unwrap();
        std::fs::create_dir_all(custom_dir_path.clone()).unwrap();

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/positive/factory-reset-status_succeeded",
        );
        std::env::set_var("FACTORY_RESET_CONFIG_FILE_PATH", config_file_path.clone());
        std::env::set_var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH", custom_dir_path);

        let mut factory_reset = FactoryReset::new().unwrap();

        assert!(factory_reset
            .command(&Command::FactoryReset(FactoryResetCommand {
                mode: FactoryResetMode::Mode1,
                preserve: vec!["foo".to_string()],
            }))
            .await
            .unwrap_err()
            .chain()
            .any(|e| e
                .to_string()
                .starts_with("unknown preserve topic received: foo")));

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
            .unwrap();

        let (tx_reported_properties, mut rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let (tx_outgoing_message, mut _rx_outgoing_message) = tokio::sync::mpsc::channel(100);

        factory_reset
            .connect_twin(tx_reported_properties, tx_outgoing_message)
            .await
            .unwrap();

        let reported: FactoryResetResult = serde_json::from_value(
            rx_reported_properties.recv().await.unwrap()["factory_reset"]["result"].clone(),
        )
        .unwrap();

        assert_eq!(
            reported,
            FactoryResetResult {
                status: FactoryResetStatus::ModeSupported,
                error: "-".to_string(),
                context: None,
                paths: vec![],
            }
        );
    }

    #[test]
    fn factory_reset_keys_test() {
        std::env::set_var(
            "FACTORY_RESET_CONFIG_FILE_PATH",
            "testfiles/positive/factory-rest.json",
        );
        assert!(FactoryReset::factory_reset_keys()
            .unwrap_err()
            .to_string()
            .starts_with("failed to open for read: "));

        let tmp_dir = tempfile::tempdir().unwrap();
        let file_path = tmp_dir.path().join("factory-reset.json");
        std::env::set_var("FACTORY_RESET_CONFIG_FILE_PATH", file_path.clone());

        std::fs::copy(
            "testfiles/positive/factory-reset.json",
            file_path.clone().as_path(),
        )
        .unwrap();

        assert!(FactoryReset::factory_reset_keys()
            .unwrap_err()
            .to_string()
            .starts_with("read factory-reset.d"));

        let custom_dir_path = tmp_dir.path().join("factory-reset.d");
        std::fs::create_dir(custom_dir_path.clone()).unwrap();
        std::env::set_var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH", custom_dir_path);

        FactoryReset::factory_reset_keys().unwrap();
    }

    #[test]
    fn factory_reset_status_test() {
        std::env::set_var("FACTORY_RESET_STATUS_FILE_PATH", "");
        assert!(FactoryReset::factory_reset_result()
            .unwrap_err()
            .to_string()
            .starts_with("failed to open for read"));

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/negative/factory-reset-status_unexpected_factory_reset_format",
        );
        assert!(FactoryReset::factory_reset_result()
            .unwrap_err()
            .to_string()
            .starts_with("failed to parse factory reset result from initramfs"));

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/positive/factory-reset-status_succeeded",
        );
        assert_eq!(
            FactoryReset::factory_reset_result().unwrap().unwrap(),
            FactoryResetResult {
                status: FactoryResetStatus::ModeSupported,
                error: "-".to_string(),
                paths: vec![],
                context: None,
            }
        );

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/positive/factory-reset-status_normal_boot",
        );
        assert!(FactoryReset::factory_reset_result().unwrap().is_none());
    }
}
