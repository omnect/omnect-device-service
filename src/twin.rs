use crate::Message;
use crate::CONSENT_DIR_PATH;
use anyhow::Context;
use anyhow::Result;
use azure_iot_sdk::client::*;
use default_env::default_env;
use log::{info, warn};
use network_interface::NetworkInterface;
use network_interface::NetworkInterfaceConfig;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::fs::OpenOptions;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub static NETWORK_NAME_FILTER: &'static str = default_env!("NETWORK_NAME_FILTER", "eth wlan");

pub struct Twin {
    tx: Arc<Mutex<Sender<Message>>>,
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
    pub fn new(tx: Arc<Mutex<Sender<Message>>>) -> Self {
        Twin { tx }
    }

    pub fn update(&mut self, state: TwinUpdateState, desired: serde_json::Value) -> Result<()> {
        self.desired_general_consent(state, desired)
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

    fn desired_general_consent(
        &mut self,
        state: TwinUpdateState,
        desired: serde_json::Value,
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
            tx: Arc::clone(&self.tx),
            report_default: true,
        };

        if let Some(consents) = match state {
            TwinUpdateState::Partial => desired["general_consent"].as_array(),
            TwinUpdateState::Complete => desired["desired"]["general_consent"].as_array(),
        } {
            let file = OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open(format!("{}/consent_conf.json", CONSENT_DIR_PATH))?;

            serde_json::to_writer_pretty(file, &json!({ "general_consent": consents }))?;
        } else {
            info!("no general consent defined in desired properties");
        }

        self.report_general_consent()?;

        guard.report_default = false;

        Ok(())
    }

    fn report_versions(&mut self) -> Result<()> {
        self.tx.lock().unwrap().send(Message::Reported(json!({
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
            .lock()
            .unwrap()
            .send(Message::Reported(serde_json::from_reader(file)?))?;

        Ok(())
    }

    fn report_user_consent(&mut self, report_consent_file: &str) -> Result<()> {
        let json: serde_json::Value =
            serde_json::from_str(fs::read_to_string(report_consent_file)?.as_str())?;

        self.tx.lock().unwrap().send(Message::Reported(json))?;

        info!("reported user consent file: {}", report_consent_file);

        Ok(())
    }

    fn report_factory_reset_status(&mut self, status: &str) -> Result<()> {
        self.tx.lock().unwrap().send(Message::Reported(json!({
            "factory_reset_status": {
                "status": status,
                "date": OffsetDateTime::now_utc().format(&Rfc3339)?.to_string(),
            }
        })))?;

        Ok(())
    }

    fn report_factory_reset_result(&mut self) -> Result<()> {
        if let Ok(output) = Command::new("sh")
            .arg("-c")
            .arg("fw_printenv factory-reset-status")
            .output()
        {
            let status = String::from_utf8(output.stdout)?;
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
                        .output()?;

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
            warn!("fw_printenv command not supported");
        }

        Ok(())
    }

    fn report_network_status(&mut self) -> Result<()> {
        #[derive(Serialize, Deserialize, Debug)]
        struct NetworkReport {
            name: String,
            addr: String,
            mac: String,
        }

        let reported_interfaces = NetworkInterface::show()?
            .iter()
            .filter(|i| {
                NETWORK_NAME_FILTER
                    .split_whitespace()
                    .any(|f| i.name.starts_with(f))
            })
            .map(|i| NetworkReport {
                name: i.name.clone(),
                addr: i
                    .addr
                    .map_or("none".to_string(), |addr| addr.ip().to_string()),
                mac: i.mac_addr.clone().unwrap_or("none".to_string()),
            })
            .collect::<Vec<NetworkReport>>();

        let t = json!({ "NetworksInterfaces": json!(reported_interfaces) });

        self.tx.lock().unwrap().send(Message::Reported(t))?;

        Ok(())
    }
}
