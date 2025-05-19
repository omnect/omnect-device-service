use crate::{
    bootloader_env,
    common::from_json_file,
    reboot_reason, systemd,
    twin::{feature::*, Feature},
    web_service,
};
use anyhow::{bail, Context, Result};
use azure_iot_sdk::client::IotMessage;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_repr::*;
use std::{collections::HashMap, env, fs::read_dir};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
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

pub struct FactoryReset {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
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
        self.report_factory_reset_keys().await?;
        self.handle_factory_reset_status().await
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.tx_reported_properties = Some(tx_reported_properties);
        self.report_factory_reset_keys().await?;
        self.handle_factory_reset_status().await
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
    const FACTORY_RESET_VERSION: u8 = 2;
    const ID: &'static str = "factory_reset";

    pub fn new() -> Self {
        FactoryReset {
            tx_reported_properties: None,
        }
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

    async fn report_factory_reset_keys(&self) -> Result<()> {
        // get keys on each call, since factory_reset.d could have changes
        let keys = FactoryReset::factory_reset_keys()?;
        web_service::publish(
            web_service::PublishChannel::FactoryResetKeysV1,
            json!({"keys": keys}),
        )
        .await;

        let Some(tx) = &self.tx_reported_properties else {
            warn!("report_factory_reset_keys: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({
            "factory_reset": {
                "keys": keys,
            }
        }))
        .await
        .context("report_factory_reset_status: send")
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
        self.report_factory_reset_status("in_progress").await?;
        if let Err(e) =
            reboot_reason::write_reboot_reason("factory-reset", "initiated by portal or API")
        {
            error!("reset_to_factory_settings: failed to write reboot reason with {e:#}");
        }
        systemd::reboot().await?;
        Ok(None)
    }

    async fn report_factory_reset_status(&self, status: &str) -> Result<()> {
        // ToDo why is that not the same format as for the cloud?
        web_service::publish(
            web_service::PublishChannel::FactoryResetStatusV1,
            json!({"factory_reset_status": status}),
        )
        .await;

        let Some(tx) = &self.tx_reported_properties else {
            warn!("report_factory_reset_status: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({
            "factory_reset": {
                "status": {
                    "status": status,
                    "date": OffsetDateTime::now_utc().format(&Rfc3339)
                    .context("report_factory_reset_status: format time to Rfc3339")?,
                }
            }
        }))
        .await
        .context("report_factory_reset_status: send")
    }

    fn factory_reset_status() -> Result<Option<&'static str>> {
        let omnect_os_initramfs_json: serde_json::Value =
            from_json_file(factory_reset_status_path!())?;

        let factory_reset = &omnect_os_initramfs_json["factory-reset"];
        anyhow::ensure!(factory_reset.is_object(), "factory-reset is not an object");

        let mut factory_reset_status = String::from("");
        let status = &factory_reset["status"];
        let error = &factory_reset["error"];
        if !status.is_null() {
            factory_reset_status = format!(
                "{}:{}",
                status.to_string().trim_matches('"'),
                error.to_string().trim_matches('"')
            );
        }

        debug!("factory_reset_status: {factory_reset_status}");
        // ToDo more stati
        match factory_reset_status.as_str() {
            "0:0" => Ok(Some("succeeded")),
            "1:-" => bail!("unexpected factory reset type"),
            "2:-" => bail!("unexpected restore setting"),
            "" => Ok(None),
            _ => bail!("unexpected factory reset status format"),
        }
    }

    async fn handle_factory_reset_status(&self) -> Result<()> {
        match Self::factory_reset_status() {
            Ok(Some(status)) => {
                info!("factory reset status: {status}");
                self.report_factory_reset_status(status).await
            }
            Ok(None) => {
                info!("factory reset status: normal boot without factory reset");
                Ok(())
            }
            Err(e) => {
                warn!("factory reset status: {e:#}");
                self.report_factory_reset_status(e.to_string().as_str())
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[tokio::test(flavor = "multi_thread")]
    async fn factory_reset_test() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("factory-reset.json");
        let custom_dir_path = temp_dir.path().join("factory-reset.d");

        std::fs::copy(
            "testfiles/positive/factory-reset.json",
            file_path.clone().as_path(),
        )
        .unwrap();
        std::fs::create_dir_all(custom_dir_path.clone()).unwrap();

        std::env::set_var("FACTORY_RESET_CONFIG_FILE_PATH", file_path.clone());
        std::env::set_var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH", custom_dir_path);

        let (tx_reported_properties, mut rx_reported_properties) = tokio::sync::mpsc::channel(100);

        let mut factory_reset = FactoryReset {
            tx_reported_properties: Some(tx_reported_properties),
        };

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

        let reported = format!("{:?}", rx_reported_properties.recv().await.unwrap());
        const UTC_REGEX: &str = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[\+-]\d{2}:\d{2})";

        let re = format!(
            "{}{}{}",
            regex::escape(r#"Object {"factory_reset": Object {"status": Object {"date": String(""#,),
            UTC_REGEX,
            regex::escape(r#""), "status": String("in_progress")}}}"#,),
        );

        let re = Regex::new(re.as_str()).unwrap();
        assert!(re.is_match(&reported));
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
        assert!(FactoryReset::factory_reset_status()
            .unwrap_err()
            .to_string()
            .starts_with("failed to open for read: "));

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/negative/factory-reset-status_unexpected_reset_type",
        );
        assert!(FactoryReset::factory_reset_status()
            .unwrap_err()
            .to_string()
            .starts_with("unexpected factory reset type"));

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/negative/factory-reset-status_unexpected_reset_settings",
        );
        assert!(FactoryReset::factory_reset_status()
            .unwrap_err()
            .to_string()
            .starts_with("unexpected restore setting"));

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/negative/factory-reset-status_unexpected_factory_reset_format",
        );
        assert!(FactoryReset::factory_reset_status()
            .unwrap_err()
            .to_string()
            .starts_with("unexpected factory reset status format"));

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/positive/factory-reset-status_succeeded",
        );
        assert_eq!(
            FactoryReset::factory_reset_status().unwrap().unwrap(),
            "succeeded"
        );

        std::env::set_var(
            "FACTORY_RESET_STATUS_FILE_PATH",
            "testfiles/positive/factory-reset-status_normal_boot",
        );
        assert!(FactoryReset::factory_reset_status().unwrap().is_none());
    }
}
