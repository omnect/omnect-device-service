#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;

use crate::consent_path;
use crate::Message;
use anyhow::{anyhow, Context, Result};
use azure_iot_sdk::client::*;
use log::{error, info, warn};
use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use once_cell::sync::OnceCell;
use serde::Serialize;
use serde_json::json;
use serde_with::skip_serializing_none;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

static INSTANCE: OnceCell<Mutex<Twin>> = OnceCell::new();

pub struct TwinInstance {
    inner: &'static Mutex<Twin>,
}

pub fn get_or_init(tx: Option<Arc<Mutex<Sender<Message>>>>) -> TwinInstance {
    if tx.is_some() {
        TwinInstance {
            inner: INSTANCE.get_or_init(|| {
                Mutex::new(Twin {
                    tx: tx,
                    ..Default::default()
                })
            }),
        }
    } else {
        TwinInstance {
            inner: INSTANCE.get().unwrap(),
        }
    }
}

struct TwinLock<'a> {
    inner: MutexGuard<'a, Twin>,
}

impl TwinInstance {
    fn lock(&self) -> TwinLock<'_> {
        TwinLock {
            inner: self.inner.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    pub fn report(&self, property: &ReportProperty) -> Result<()> {
        self.lock().inner.report(property)
    }

    pub fn update(&self, state: TwinUpdateState, desired: serde_json::Value) -> Result<()> {
        self.lock().inner.update(state, desired)
    }
}

#[derive(Default)]
struct Twin {
    tx: Option<Arc<Mutex<Sender<Message>>>>,
    include_network_filter: Option<Vec<String>>,
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
    fn update(&mut self, state: TwinUpdateState, desired: serde_json::Value) -> Result<()> {
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

    fn report(&mut self, property: &ReportProperty) -> Result<()> {
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
        desired_consents: Option<&Vec<serde_json::Value>>,
    ) -> Result<()> {
        if desired_consents.is_some() {
            let mut new_consents = desired_consents
                .unwrap()
                .iter()
                .map(|e| match (e.is_string(), e.as_str()) {
                    (true, Some(s)) => Ok(s.to_string().to_lowercase()),
                    _ => Err(anyhow!("cannot parse string from new_consents json.")
                        .context("update_general_consent: parse desired_consents")),
                })
                .collect::<Result<Vec<String>>>()?;

            // enforce entries only exists once
            new_consents.sort_by_key(|name| name.to_string());
            new_consents.dedup();

            let saved_consents: serde_json::Value = serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("update_general_consent: open consent_conf.json for read")?,
            )
            .context("update_general_consent: serde_json::from_reader")?;

            let saved_consents = saved_consents["general_consent"]
                .as_array()
                .context("update_general_consent: general_consent array malformed")?
                .iter()
                .map(|e| match (e.is_string(), e.as_str()) {
                    (true, Some(s)) => Ok(s.to_string().to_lowercase()),
                    _ => Err(anyhow!("cannot parse string from saved_consents json.")
                        .context("update_general_consent: parse saved_consents")),
                })
                .collect::<Result<Vec<String>>>()?;

            // check if consents changed (current desired vs. saved)
            if new_consents.eq(&saved_consents) {
                info!("desired general_consent didn't change");
                return Ok(());
            }

            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("update_general_consent: open consent_conf.json for write")?,
                &json!({ "general_consent": new_consents }),
            )
            .context("update_general_consent: serde_json::to_writer_pretty")?;
        } else {
            info!("no general consent defined in desired properties. current general_consent is reported.");
        };

        self.report_general_consent()
            .context("update_general_consent: report_general_consent")
    }

    fn report_versions(&mut self) -> Result<()> {
        let version = json!({
            "module-version": env!("CARGO_PKG_VERSION"),
            "azure-sdk-version": IotHubClient::get_sdk_version_string()
        });

        self.report_impl(version.clone())
            .context("report_versions: report_impl")
            .map_err(|err| err.into())
    }

    fn report_general_consent(&mut self) -> Result<()> {
        self.report_impl(
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("report_general_consent: open consent_conf.json fo read")?,
            )
            .context("report_general_consent: serde_json::from_reader")?,
        )
        .context("report_general_consent: report_impl")
        .map_err(|err| err.into())
    }

    fn report_user_consent(&mut self, report_consent_file: &str) -> Result<()> {
        self.report_impl(
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(report_consent_file)
                    .context("report_user_consent: open report_consent_file fo read")?,
            )
            .context("report_user_consent: serde_json::from_reader")?,
        )
        .context("report_user_consent: report_impl")
        .map_err(|err| err.into())
    }

    fn report_factory_reset_status(&mut self, status: &str) -> Result<()> {
        self.report_impl(json!({
            "factory_reset_status": {
                "status": status,
                "date": OffsetDateTime::now_utc().format(&Rfc3339)
                .context("report_factory_reset_status: format time to Rfc3339")?.to_string(),
            }
        }))
        .context("report_factory_reset_status: report_impl")
        .map_err(|err| err.into())
    }

    fn report_factory_reset_result(&mut self) -> Result<()> {
        if let Ok(output) = Command::new("sh")
            .arg("-c")
            .arg("fw_printenv factory-reset-status")
            .output()
        {
            let status = String::from_utf8(output.stdout).unwrap_or_else(|e| {
                error!("report_factory_reset_result: {:#?}", e);
                String::from("")
            });
            let vec: Vec<&str> = status.split("=").collect();

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

    fn update_include_network_filter(
        &mut self,
        include_network_filter: Option<&Vec<serde_json::Value>>,
    ) -> Result<()> {
        if include_network_filter.is_none() {
            self.include_network_filter.take();
            return self
                .report_impl(json!({ "network_interfaces": json!(null) }))
                .context("report_network_status: report_impl");
        }

        let mut new_include_network_filter: Vec<String> = include_network_filter
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
            .collect();

        // enforce entries only exist once
        new_include_network_filter.sort();
        new_include_network_filter.dedup();

        // check if desired include_network_filter changed
        if self.include_network_filter.is_none()
            || self
                .include_network_filter
                .as_ref()
                .unwrap()
                .ne(&new_include_network_filter)
        {
            self.include_network_filter
                .replace(new_include_network_filter);
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
                self.include_network_filter
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|f| {
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

        self.report_impl(json!({
            "network_interfaces": json!(interfaces.into_values().collect::<Vec<NetworkReport>>())
        }))
        .context("report_network_status")
        .map_err(|err| err.into())
    }

    fn report_impl(&mut self, value: serde_json::Value) -> Result<()> {
        info!("report: \n{:?}", value);

        self.tx
            .as_ref()
            .ok_or(anyhow::anyhow!("tx channel missing"))?
            .lock()
            .unwrap()
            .send(Message::Reported(value))
            .map_err(|err| err.into())
    }
}
