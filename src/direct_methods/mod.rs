#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;

use crate::consent_path;
use crate::twin;
use anyhow::{Context, Result};
use azure_iot_sdk::client::*;
use lazy_static::{__Deref, lazy_static};
use log::info;
use serde_json::json;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use twin::ReportProperty;

lazy_static! {
    static ref SETTINGS_MAP: HashMap<&'static str, &'static str> = {
        let mut map = HashMap::new();
        map.insert("wifi", "/etc/wpa_supplicant/wpa_supplicant-wlan0.conf");
        map
    };
}

pub fn get_direct_methods() -> Option<DirectMethodMap> {
    let mut methods = DirectMethodMap::new();
    methods.insert(
        String::from("factory_reset"),
        IotHubClient::make_direct_method(move |in_json| reset_to_factory_settings(in_json)),
    );
    methods.insert(String::from("user_consent"), Box::new(user_consent));
    methods.insert(String::from("reboot"), Box::new(reboot));
    methods.insert(
        String::from("refresh_network_status"),
        IotHubClient::make_direct_method(move |in_json| refresh_network_status(in_json)),
    );

    Some(methods)
}

pub fn reset_to_factory_settings(in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("factory reset requested");

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

pub fn user_consent(in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("user consent requested");

    match serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(in_json) {
        Ok(map) if map.len() == 1 && map.values().next().unwrap().is_string() => {
            let (component, version) = map.iter().next().unwrap();
            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(format!(
                        "{}/{}/user_consent.json",
                        consent_path!(),
                        component
                    ))
                    .context("user_consent: open user_consent.json for write")?,
                &json!({ "consent": version }),
            )
            .context("user_consent: serde_json::to_writer_pretty")?;

            Ok(None)
        }
        _ => anyhow::bail!("unexpected parameter format"),
    }
}

pub fn reboot(_in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("reboot requested");

    OpenOptions::new()
        .write(true)
        .create(false)
        .truncate(true)
        .open("/run/omnect-device-service/reboot-trigger")?;

    Ok(None)
}

pub fn refresh_network_status(_in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("network status requested");

    twin::get_or_init(None).report(&ReportProperty::NetworkStatus)?;

    Ok(None)
}
