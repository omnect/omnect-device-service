use super::super::systemd;
use super::{Feature, FeatureState};
use crate::twin;
use crate::twin::Twin;
use anyhow::{anyhow, Context, Result};
use futures_executor::block_on;
use lazy_static::{__Deref, lazy_static};
use log::{error, info, warn};
use serde_json::json;
use std::any::Any;
use std::collections::HashMap;
use std::env;
#[cfg(not(test))]
use std::process::Command;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

lazy_static! {
    static ref SETTINGS_MAP: HashMap<&'static str, &'static str> = {
        let mut map = HashMap::new();
        map.insert("wifi", "/etc/wpa_supplicant/wpa_supplicant-wlan0.conf");
        map
    };
}

pub fn reset_to_factory_settings(in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    twin::get_or_init(None).exec(|twin| {
        twin.get_feature::<FactoryReset>()?
            .reset_to_factory_settings(in_json.to_owned())
    })
}

#[derive(Default)]
pub struct FactoryReset {
    state: FeatureState,
}

impl Feature for FactoryReset {
    fn get_name(&self) -> String {
        Self::ID.to_string()
    }

    fn get_version(&self) -> u8 {
        Self::FACTORY_RESET_VERSION
    }

    fn is_enabled(&self) -> bool {
        !env::vars().any(|(k, v)| k == "SUPPRESS_FACTORY_RESET" && v == "true")
    }

    fn report_initial_state(&self) -> Result<()> {
        self.report_factory_reset_result()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn get_state_mut(&mut self) -> &mut FeatureState {
        &mut self.state
    }

    fn get_state(&self) -> &FeatureState {
        &self.state
    }
}

impl FactoryReset {
    const FACTORY_RESET_VERSION: u8 = 1;
    const ID: &'static str = "factory_reset";

    pub fn reset_to_factory_settings(
        &self,
        in_json: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("factory reset requested: {}", in_json);

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
                        paths.push(SETTINGS_MAP.get(s).unwrap().deref());
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
                anyhow::ensure!(
                    self.exec_cmd(vec![
                        "fw_setenv",
                        "factory-reset-restore-list",
                        restore_paths.as_str()
                    ])?,
                    "failed to set factory-reset-restore-list in u-boot env"
                );

                anyhow::ensure!(
                    self.exec_cmd(vec!["fw_setenv", "factory-reset", &reset_type.to_string()])?,
                    "failed to set factory-reset type in u-boot env"
                );

                self.report_factory_reset_status("in_progress")?;

                block_on(async { systemd::reboot().await })?;

                Ok(None)
            }
            _ => anyhow::bail!("reset type missing or not supported"),
        }
    }

    fn report_factory_reset_status(&self, status: &str) -> Result<()> {
        self.ensure()?;

        Twin::report_impl(
            self.get_tx(),
            json!({
                "factory_reset": {
                    "status": {
                        "status": status,
                        "date": OffsetDateTime::now_utc().format(&Rfc3339)
                        .context("report_factory_reset_status: format time to Rfc3339")?,
                    }
                }
            }),
        )
        .context("report_factory_reset_status: report_impl")
    }

    fn report_factory_reset_result(&self) -> Result<()> {
        self.ensure()?;

        if let Ok(status) = self.get_factory_reset_status() {
            let status: Vec<&str> = status.iter().map(AsRef::as_ref).collect();
            let status = match status[..] {
                ["factory-reset-status", "0:0\n"] => Ok(("succeeded", true)),
                ["factory-reset-status", "1:-\n"] => Ok(("unexpected factory reset type", true)),
                ["factory-reset-status", "2:-\n"] => {
                    Ok(("unexpected restore settings error", true))
                }
                ["factory-reset-status", "\n"] => Ok(("normal boot without factory reset", false)),
                ["factory-reset-status", _] => Ok(("failed", true)),
                _ => Err(anyhow!("unexpected factory reset result format")),
            };

            match status {
                Ok((update_twin, true)) => {
                    self.report_factory_reset_status(update_twin)?;
                    anyhow::ensure!(
                        self.exec_cmd(vec!["fw_setenv", "factory-reset-status"])?,
                        "failed to reset factory-reset-status"
                    );

                    info!("factory reset result: {}", update_twin);
                }
                Ok((update_twin, false)) => {
                    info!("factory reset result: {}", update_twin);
                }
                Err(update_twin) => {
                    warn!("factory reset result: {}", update_twin);
                }
            };
        } else {
            error!("fw_printenv command not supported");
        }

        Ok(())
    }

    #[cfg(not(test))]
    fn get_factory_reset_status(&self) -> Result<Vec<String>> {
        let output = Command::new("sudo")
            .arg("fw_printenv")
            .arg("factory-reset-status")
            .output()?;

        anyhow::ensure!(
            output.status.success(),
            "failed to get factory-reset-status"
        );

        let status = String::from_utf8(output.stdout).unwrap_or_else(|e| {
            error!("report_factory_reset_result: {:#?}", e);
            String::from("")
        });

        Ok(status.split('=').map(String::from).collect())
    }

    #[cfg(test)]
    #[allow(unreachable_patterns)]
    fn get_factory_reset_status(&self) -> Result<Vec<String>> {
        match std::env::var("TEST_FACTORY_RESET_RESULT")
            .unwrap_or("succeeded".to_string())
            .as_str()
        {
            "unexpected_factory_reset_result_format" => {
                Ok(vec!["factory-reset-status".to_string()])
            }
            "normal_boot_without_factory_reset" => {
                Ok(vec!["factory-reset-status".to_string(), "\n".to_string()])
            }
            "unexpected_restore_settings_error" => Ok(vec![
                "factory-reset-status".to_string(),
                "2:-\n".to_string(),
            ]),
            "unexpected_factory_reset_type" => Ok(vec![
                "factory-reset-status".to_string(),
                "1:-\n".to_string(),
            ]),
            _ | "succeeded" => Ok(vec![
                "factory-reset-status".to_string(),
                "0:0\n".to_string(),
            ]),
        }
    }

    #[cfg(not(test))]
    fn exec_cmd(&self, args: Vec<&str>) -> Result<bool> {
        Ok(Command::new("sudo").args(args).status()?.success())
    }

    #[cfg(test)]
    fn exec_cmd(&self, _args: Vec<&str>) -> Result<bool> {
        Ok(true)
    }
}
