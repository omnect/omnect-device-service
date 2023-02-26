mod common;
mod consent;
mod factory_reset;
#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;
mod network_status;
use crate::Message;
use anyhow::{Context, Result};
use azure_iot_sdk::client::*;
use log::{info, warn};
use once_cell::sync::OnceCell;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};

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

    pub fn get_direct_methods(&self) -> Option<DirectMethodMap> {
        let mut methods = DirectMethodMap::new();
        methods.insert(
            String::from("factory_reset"),
            IotHubClient::make_direct_method(move |in_json| {
                factory_reset::reset_to_factory_settings(in_json)
            }),
        );
        methods.insert(
            String::from("user_consent"),
            Box::new(consent::user_consent),
        );
        methods.insert(String::from("reboot"), Box::new(common::reboot));
        methods.insert(
            String::from("refresh_network_status"),
            IotHubClient::make_direct_method(move |in_json| {
                network_status::refresh_network_status(in_json)
            }),
        );

        Some(methods)
    }

    pub fn cloud_message(&self, msg: IotMessage) {
        warn!(
            "received unexpected C2D message with \n body: {:?}\n properties: {:?} \n system properties: {:?}",
            std::str::from_utf8(&msg.body).unwrap(),
            msg.properties,
            msg.system_properties
        );
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
