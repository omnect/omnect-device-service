use super::super::bootloader_env;
use super::super::systemd;
use super::Feature;
use crate::web_service;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{info, warn};
use serde_json::json;
use std::{any::Any, collections::HashMap, env};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::mpsc::Sender;

macro_rules! factory_reset_status_path {
    () => {{
        static FACTORY_RESET_STATUS_FILE_PATH_DEFAULT: &'static str =
            "/run/omnect-device-service/factory-reset-status";
        std::env::var("FACTORY_RESET_STATUS_FILE_PATH")
            .unwrap_or(FACTORY_RESET_STATUS_FILE_PATH_DEFAULT.to_string())
    }};
}

pub struct FactoryReset {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    settings: HashMap<&'static str, String>,
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
    const FACTORY_RESET_VERSION: u8 = 1;
    const ID: &'static str = "factory_reset";
    const WPA_SUPPLICANT_PATH_DEFAULT: &'static str = "/etc/wpa_supplicant.conf";

    pub fn new() -> Self {
        let wpa_supplicant_path = if let Ok(path) = std::env::var("WPA_SUPPLICANT_DIR_PATH") {
            format!("{path}/wpa_supplicant.conf")
        } else {
            Self::WPA_SUPPLICANT_PATH_DEFAULT.to_string()
        };

        let settings = HashMap::from([("wifi", wpa_supplicant_path)]);

        FactoryReset {
            tx_reported_properties: None,
            settings,
        }
    }

    pub async fn reset_to_factory_settings(
        &self,
        in_json: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("factory reset requested: {in_json}");

        self.ensure()?;

        let restore_paths = match in_json["restore_settings"].as_array() {
            Some(settings) => {
                let mut paths = vec![];
                let mut settings: Vec<&str> =
                    settings.iter().map(|v| v.as_str().unwrap()).collect();

                // enforce a value exists only once
                settings.sort();
                settings.dedup();

                while let Some(s) = settings.pop() {
                    if self.settings.contains_key(s) {
                        let path = self.settings.get(s).unwrap();

                        if let Ok(true) = std::path::Path::new(&path).try_exists() {
                            paths.push(path.clone());
                        } else {
                            anyhow::bail!("invalid restore path received: {path}");
                        }
                    } else {
                        anyhow::bail!("unknown restore setting received: {s}");
                    }
                }

                paths.join(";")
            }
            _ => String::from(""),
        };

        match &in_json["type"].as_u64() {
            Some(reset_type) => {
                bootloader_env::set("factory-reset-restore-list", restore_paths.as_str())?;
                bootloader_env::set("factory-reset", &reset_type.to_string())?;

                self.report_factory_reset_status("in_progress").await?;

                systemd::reboot().await?;

                Ok(None)
            }
            _ => anyhow::bail!("reset type missing or not supported"),
        }
    }

    async fn report_factory_reset_status(&self, status: &str) -> Result<()> {
        self.ensure()?;

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
        let Ok(factory_reset_status) = std::fs::read_to_string(factory_reset_status_path!()) else {
            bail!(
                "factory reset status file missing: {}",
                factory_reset_status_path!()
            );
        };

        match factory_reset_status.trim_end() {
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
            .starts_with("factory reset status file missing"));

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
