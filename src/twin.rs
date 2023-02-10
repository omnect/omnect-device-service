use crate::Message;
use crate::CONSENT_DIR_PATH;
use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use azure_iot_sdk::client::*;
use log::{error, info, warn};
use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use once_cell::sync::Lazy;
use serde::Serialize;
use serde_json::json;
use serde_with::skip_serializing_none;
use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub static TWIN: Lazy<Mutex<Twin>> = Lazy::new(|| {
    Mutex::new(Twin {
        ..Default::default()
    })
});

#[derive(Default)]
pub struct Twin {
    tx: Option<Arc<Mutex<Sender<Message>>>>,
    include_network_filter: Vec<String>,
}

pub enum ReportProperty<'a> {
    Versions,
    GeneralConsent,
    UserConsent(&'a str),
    FactoryResetStatus(&'a str),
    FactoryResetResult,
    NetworkStatus,
}

impl Twin {
    pub fn set_sender(&mut self, tx: Arc<Mutex<Sender<Message>>>) {
        self.tx = Some(tx);
    }

    pub fn update(&mut self, state: TwinUpdateState, desired: serde_json::Value) -> Result<()> {
        match state {
            TwinUpdateState::Partial => {
                self.update_general_consent(desired["general_consent"].as_array())?;
                self.update_include_network_filter(desired["include_network_filter"].as_array())
            }
            TwinUpdateState::Complete => {
                self.update_general_consent(desired["desired"]["general_consent"].as_array())?;
                self.update_include_network_filter(
                    desired["desired"]["include_network_filter"].as_array(),
                )
            }
        }
    }

    pub fn report(&mut self, property: &ReportProperty) -> Result<()> {
        match property {
            ReportProperty::Versions => self.report_versions().context("Couldn't report version"),
            ReportProperty::GeneralConsent => self
                .report_general_consent()
                .context("Couldn't report general consent"),
            ReportProperty::UserConsent(file) => self
                .report_user_consent(file)
                .context("Couldn't report user consent"),
            ReportProperty::FactoryResetStatus(status) => self
                .report_factory_reset_status(status)
                .context("Couldn't report factory reset status"),
            ReportProperty::FactoryResetResult => self
                .report_factory_reset_result()
                .context("Couldn't report factory reset result"),
            ReportProperty::NetworkStatus => self
                .report_network_status()
                .context("Couldn't report network status"),
        }
    }

    fn update_general_consent(
        &mut self,
        new_consents: Option<&Vec<serde_json::Value>>,
    ) -> Result<()> {
        struct Guard {
            tx: Arc<Mutex<Sender<Message>>>,
            report_default: bool,
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                if self.report_default {
                    self.tx
                        .lock()
                        .unwrap()
                        .send(Message::Reported(json!({ "general_consent": null })))
                        .unwrap()
                }
            }
        }

        let mut guard = Guard {
            tx: Arc::clone(
                &self
                    .tx
                    .as_ref()
                    .ok_or(anyhow::anyhow!("sender missing").context("update_general_consent"))?
                    .to_owned(),
            ),
            report_default: true,
        };

        let mut new_consents = if new_consents.is_some() {
            new_consents
                .unwrap()
                .iter()
                .filter(|e| {
                    if !e.is_string() {
                        error!(
                            "unexpected format in desired general_consent. ignore: {}",
                            e.to_string()
                        );
                    }
                    e.is_string()
                })
                .map(|e| {
                    if let Some(s) = e.as_str() {
                        Ok(s.to_string().to_lowercase())
                    } else {
                        Err(anyhow!("cannot parse str from new_consents json.")
                            .context("update_general_consent"))
                    }
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            info!("no or malformed general consent defined in desired properties. default to empty array.");
            vec![]
        };

        // enforce entries only exists once
        new_consents.sort_by_key(|name| name.to_string());
        new_consents.dedup();

        let saved_consents: serde_json::Value = serde_json::from_reader(
            OpenOptions::new()
                .read(true)
                .create(false)
                .open(format!("{}/consent_conf.json", CONSENT_DIR_PATH))
                .context("update_general_consent")?,
        )?;

        let saved_consents: Vec<&str> = saved_consents["general_consent"]
            .as_array()
            .context("update_general_consent: general_consent array malformed")?
            .iter()
            .map(|e| {
                e.as_str().ok_or_else(|| {
                    anyhow::anyhow!("cannot parse str from saved_consents json.")
                        .context("update_general_consent")
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // check if consents changed (current desired vs. saved)
        if new_consents.ne(&saved_consents) {
            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(format!("{}/consent_conf.json", CONSENT_DIR_PATH))
                    .context("update_general_consent")?,
                &json!({ "general_consent": new_consents }),
            )?;

            self.report_general_consent()?;
        } else {
            info!("desired general_consent didn't change")
        }

        guard.report_default = false;

        Ok(())
    }

    fn report_versions(&mut self) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or(anyhow::anyhow!("sender missing").context("report_versions"))?
            .lock()
            .unwrap()
            .send(Message::Reported(json!({
                "module-version": env!("CARGO_PKG_VERSION"),
                "azure-sdk-version": IotHubClient::get_sdk_version_string()
            })))?;

        Ok(())
    }

    fn report_general_consent(&mut self) -> Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .create(false)
            .open(format!("{}/consent_conf.json", CONSENT_DIR_PATH))?;

        self.tx
            .as_ref()
            .ok_or(anyhow::anyhow!("sender missing").context("report_general_consent"))?
            .lock()
            .unwrap()
            .send(Message::Reported(
                serde_json::from_reader(file).context("report_general_consent")?,
            ))?;

        Ok(())
    }

    fn report_user_consent(&mut self, report_consent_file: &str) -> Result<()> {
        let json: serde_json::Value =
            serde_json::from_str(fs::read_to_string(report_consent_file)?.as_str())
                .context("report_user_consent")?;

        self.tx
            .as_ref()
            .ok_or(anyhow::anyhow!("sender missing").context("report_user_consent"))?
            .lock()
            .unwrap()
            .send(Message::Reported(json))
            .context("report_user_consent")?;

        info!("reported user consent file: {}", report_consent_file);

        Ok(())
    }

    fn report_factory_reset_status(&mut self, status: &str) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or(anyhow::anyhow!("sender missing").context("report_factory_reset_status"))?
            .lock()
            .unwrap()
            .send(Message::Reported(json!({
                "factory_reset_status": {
                    "status": status,
                    "date": OffsetDateTime::now_utc().format(&Rfc3339)?.to_string(),
                }
            })))
            .context("report_factory_reset_status")?;

        Ok(())
    }

    fn report_factory_reset_result(&mut self) -> Result<()> {
        if let Ok(output) = Command::new("sh")
            .arg("-c")
            .arg("fw_printenv factory-reset-status")
            .output()
        {
            let status = String::from_utf8(output.stdout).context("report_factory_reset_result")?;
            let vec: Vec<&str> = status.split("=").collect();

            let status = match vec[..] {
                ["factory-reset-status", "0:0\n"] => Ok(("succeeded", true)),
                ["factory-reset-status", "1:-\n"] => Ok(("unexpected factory reset type", true)),
                ["factory-reset-status", "2:-\n"] => {
                    Ok(("unexpected restore settings error", true))
                }
                ["factory-reset-status", "\n"] => Ok(("normal boot without factory reset", false)),
                ["factory-reset-status", _] => Ok(("failed", true)),
                _ => Err("unexpected factory reset result format"),
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

    fn update_include_network_filter(
        &mut self,
        include_network_filter: Option<&Vec<serde_json::Value>>,
    ) -> Result<()> {
        let mut new_include_network_filter = if include_network_filter.is_some() {
            include_network_filter
                .unwrap()
                .iter()
                .filter(|e| {
                    if !e.is_string() {
                        error!(
                            "unexpected format in desired include_network_filter. ignore: {}",
                            e.to_string()
                        );
                    }
                    e.is_string()
                })
                .map(|e| e.as_str().unwrap().to_string().to_lowercase())
                .collect()
        } else {
            vec!["*"]
        };

        // enforce entries only exists once
        new_include_network_filter.sort();
        new_include_network_filter.dedup();

        // check if desired include_network_filter changed
        if self.include_network_filter.ne(&new_include_network_filter) {
            self.include_network_filter = new_include_network_filter;
            self.report_network_status()
        } else {
            info!("desired include_network_filter didn't change");
            Ok(())
        }
    }

    fn report_network_status(&mut self) -> Result<()> {
        #[skip_serializing_none]
        #[derive(Serialize)]
        struct NetworkReport {
            #[serde(default)]
            name: String,
            mac: String,
            addr_v4: Option<Vec<String>>,
            addr_v6: Option<Vec<String>>,
        }

        let mut interfaces: HashMap<String, NetworkReport> = HashMap::new();

        NetworkInterface::show()
            .context("report_network_status")?
            .iter()
            .filter(|i| {
                self.include_network_filter.iter().any(|f| {
                    let name = i.name.to_lowercase();
                    match (f.starts_with("*"), f.ends_with("*"), f.len()) {
                        (_, _, 0) => false,                                     // ""
                        (a, b, 1) if a || b => true,                            // "*"
                        (true, true, len) => name.contains(&f[1..len - 1]),     // ""*...*"
                        (true, false, len) => name.ends_with(&f[1..len]),       // "*..."
                        (false, true, len) => name.starts_with(&f[0..len - 1]), // "...*"
                        _ => name.eq(f),                                        // "..."
                    }
                })
            })
            .for_each(|i| {
                let entry = interfaces.entry(i.name.clone()).or_insert(NetworkReport {
                    addr_v4: None,
                    addr_v6: None,
                    mac: i.mac_addr.clone().unwrap_or_else(|| "none".to_string()),
                    name: i.name.clone(),
                });

                match i.addr {
                    Some(Addr::V4(addr)) => entry
                        .addr_v4
                        .get_or_insert(vec![])
                        .push(addr.ip.to_string()),
                    Some(Addr::V6(addr)) => entry
                        .addr_v6
                        .get_or_insert(vec![])
                        .push(addr.ip.to_string()),
                    None => error!("report_network_status: ip address is missing"),
                };
            });

        self.tx
            .as_ref()
            .ok_or(anyhow::anyhow!("sender missing").context("report_network_status"))?
            .lock()
            .unwrap()
            .send(Message::Reported(json!({
                "network_interfaces":
                    json!(interfaces.into_values().collect::<Vec<NetworkReport>>())
            })))
            .context("report_network_status")?;

        Ok(())
    }
}
