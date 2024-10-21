use super::super::bootloader_env;
use super::super::systemd;
use super::Feature;
use crate::web_service;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{debug, info, warn};
use serde_json::json;
use std::{any::Any, collections::HashMap, env, fs::File, fs::read_dir, io::BufReader};
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


pub struct FactoryReset {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
}

#[async_trait(?Send)]
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

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.ensure()?;

        self.tx_reported_properties = Some(tx_reported_properties);

        self.report_factory_reset_keys().await?;

        match Self::factory_reset_status() {
            Ok(Some(status)) => {
                info!("factory reset status: {status}");
                self.report_factory_reset_status(status).await?
            }
            Ok(None) => {
                info!("factory reset status: normal boot without factory reset");
            }
            Err(e) => {
                warn!("factory reset status: {e}");
                self.report_factory_reset_status(e.to_string().as_str())
                    .await?
            }
        };

        Ok(())
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

    fn factory_reset_keys() -> Result<Vec<String>>
    {
        let key_value_map: HashMap<String, serde_json::Value> = serde_json::from_reader(BufReader::new(
            File::open(factory_reset_config_path!()).context("open factory-reset.json")?,
        ))
        .context("parsing factory reset config")?;

        let mut keys: Vec<String> = key_value_map.into_keys().collect();
        if ! read_dir(factory_reset_custom_config_dir_path!()).context("read factory-reset.d")?.next().is_none(){
            keys.push(String::from("applications"))
        }
        Ok(keys)
    }

    async fn report_factory_reset_keys(&self) -> Result<()> {
        // get keys on each call, since factory_reset.d could have changes
        let keys = FactoryReset::factory_reset_keys()?;
        web_service::publish(
            web_service::PublishChannel::FactoryResetKeys,
            json!({"keys": keys}),
        )
        .await
        .context("report_factory_reset_keys: publish")?;

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

    pub async fn reset_to_factory_settings(
        &self,
        in_json: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("factory reset requested: {in_json}");

        self.ensure()?;

        // ToDo ensure mode?
        bootloader_env::set("factory-reset", &in_json.to_string())?;
        self.report_factory_reset_status("in_progress").await?;
        systemd::reboot().await?;
        Ok(None)
    }

    async fn report_factory_reset_status(&self, status: &str) -> Result<()> {
        // ToDo why is that not the same format as for the cloud?
        web_service::publish(
            web_service::PublishChannel::FactoryResetResult,
            json!({"factory-reset-status": status}),
        )
        .await
        .context("report_factory_reset_status: publish")?;

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
        let omnect_os_initramfs_json: serde_json::Value = serde_json::from_reader(BufReader::new(
            File::open(factory_reset_status_path!()).context("open omnect-os-initramfs.json")?,
        ))
        .context("parsing factory reset status")?;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_reset_status_test() {
        std::env::set_var("FACTORY_RESET_STATUS_FILE_PATH", "");
        assert!(FactoryReset::factory_reset_status()
            .unwrap_err()
            .to_string()
            .starts_with("open omnect-os-initramfs.json"));

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
