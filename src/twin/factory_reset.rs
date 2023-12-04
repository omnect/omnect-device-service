use super::super::bootloader_env::bootloader_env::{
    bootloader_env, {set_bootloader_env, unset_bootloader_env},
};
use super::super::systemd;
use super::Feature;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use lazy_static::lazy_static;
use log::{error, info, warn};
use serde_json::json;
use std::{any::Any, collections::HashMap, env};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::mpsc::Sender;

lazy_static! {
    static ref SETTINGS_MAP: HashMap<&'static str, &'static str> = {
        let mut map = HashMap::new();
        map.insert("wifi", "/etc/wpa_supplicant/wpa_supplicant-wlan0.conf");
        map
    };
}

pub struct FactoryReset {
    tx_reported_properties: Sender<serde_json::Value>,
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

    async fn report_initial_state(&self) -> Result<()> {
        self.report_factory_reset_result().await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl FactoryReset {
    const FACTORY_RESET_VERSION: u8 = 1;
    const ID: &'static str = "factory_reset";

    pub fn new(tx_reported_properties: Sender<serde_json::Value>) -> Self {
        FactoryReset {
            tx_reported_properties,
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
                    if SETTINGS_MAP.contains_key(s) {
                        paths.push(*SETTINGS_MAP.get(s).unwrap());
                    } else {
                        anyhow::bail!("unknown restore setting received");
                    }
                }

                paths.join(";")
            }
            _ => String::from(""),
        };

        match &in_json["type"].as_u64() {
            Some(reset_type) => {
                set_bootloader_env("factory-reset-restore-list", restore_paths.as_str())?;
                set_bootloader_env("factory-reset", &reset_type.to_string())?;

                self.report_factory_reset_status("in_progress").await?;

                systemd::reboot().await?;

                Ok(None)
            }
            _ => anyhow::bail!("reset type missing or not supported"),
        }
    }

    async fn report_factory_reset_status(&self, status: &str) -> Result<()> {
        self.ensure()?;

        self.tx_reported_properties
            .send(json!({
                "factory_reset": {
                    "status": {
                        "status": status,
                        "date": OffsetDateTime::now_utc().format(&Rfc3339)
                        .context("report_factory_reset_status: format time to Rfc3339")?,
                    }
                }
            }))
            .await
            .context("report_factory_reset_status: report_impl")
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
                    unset_bootloader_env("factory-reset-status")?;

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
            bootloader_env("factory-reset-status")
        }
    }
}
