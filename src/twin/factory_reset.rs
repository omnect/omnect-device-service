use super::Twin;
use crate::{twin, ReportProperty};
use anyhow::{anyhow, Context, Result};
use lazy_static::{__Deref, lazy_static};
use log::{error, info, warn};
use serde_json::json;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
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
    info!("factory reset requested: {}", in_json);

    let restore_paths = match in_json["restore_settings"].as_array() {
        Some(settings) => {
            let mut paths = vec![];
            let mut settings: Vec<&str> = settings.iter().map(|v| v.as_str().unwrap()).collect();

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
            OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open("/run/omnect-device-service/factory-reset-restore-list")?
                .write_all(restore_paths.as_bytes())?;

            OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open("/run/omnect-device-service/factory-reset-trigger")?
                .write_all(reset_type.to_string().as_bytes())?;

            twin::get_or_init(None).report(&ReportProperty::FactoryResetStatus("in_progress"))?;

            Ok(None)
        }
        _ => anyhow::bail!("reset type missing or not supported"),
    }
}

impl Twin {
    pub fn report_factory_reset_status(&mut self, status: &str) -> Result<()> {
        self.report_impl(json!({
            "factory_reset_status": {
                "status": status,
                "date": OffsetDateTime::now_utc().format(&Rfc3339)
                .context("report_factory_reset_status: format time to Rfc3339")?,
            }
        }))
        .context("report_factory_reset_status: report_impl")
    }

    pub fn report_factory_reset_result(&mut self) -> Result<()> {
        if let Ok(output) = Command::new("sh")
            .arg("-c")
            .arg("fw_printenv factory-reset-status")
            .output()
        {
            let status = String::from_utf8(output.stdout).unwrap_or_else(|e| {
                error!("report_factory_reset_result: {:#?}", e);
                String::from("")
            });
            let vec: Vec<&str> = status.split('=').collect();

            let status = match vec[..] {
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
                    Command::new("sh")
                        .arg("-c")
                        .arg("fw_setenv factory-reset-status")
                        .output()
                        .context("report_factory_reset_result")?;

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
}
