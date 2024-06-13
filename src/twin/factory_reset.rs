use super::super::bootloader_env;
use super::super::systemd;
use super::Feature;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{error, info, warn};
use serde_json::json;
use std::{any::Any, collections::HashMap, env};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::mpsc::Sender;

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

        self.report_factory_reset_result().await
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

    async fn report_factory_reset_result(&self) -> Result<()> {
        self.ensure()?;

        if let Ok(status) = self.factory_reset_status() {
            let status = match status.as_str() {
                "0:0" => Ok(("succeeded", true)),
                "1:-" => Ok(("unexpected factory reset type", true)),
                "2:-" => Ok(("unexpected restore settings error", true)),
                "" => Ok(("normal boot without factory reset", false)),
                _ => Err(anyhow!("unexpected factory reset result format")),
            };

            match status {
                Ok((update_twin, true)) => {
                    self.report_factory_reset_status(update_twin).await?;
                    bootloader_env::unset("factory-reset-status")?;

                    info!("factory reset result: {update_twin}");
                }
                Ok((update_twin, false)) => {
                    info!("factory reset result: {update_twin}");
                }
                Err(update_twin) => {
                    warn!("factory reset result: {update_twin}");
                }
            };
        } else {
            error!("getting factory reset status failed");
        }

        Ok(())
    }

    #[allow(unreachable_patterns, clippy::wildcard_in_or_patterns)]
    fn factory_reset_status(&self) -> Result<String> {
        if cfg!(feature = "mock") {
            match std::env::var("TEST_FACTORY_RESET_RESULT")
                .unwrap_or_else(|_| "succeeded".to_string())
                .as_str()
            {
                "unexpected_factory_reset_result_format" => Ok("unexpected".to_string()),
                "normal_boot_without_factory_reset" => Ok("".to_string()),
                "unexpected_restore_settings_error" => Ok("2:-".to_string()),
                "unexpected_factory_reset_type" => Ok("1:-".to_string()),
                _ | "succeeded" => Ok("0:0".to_string()),
            }
        } else {
            bootloader_env::get("factory-reset-status")
        }
    }
}
