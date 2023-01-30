use crate::twin;
use crate::Message;
use crate::CONSENT_DIR_PATH;
use anyhow::Result;
use azure_iot_sdk::client::*;
use lazy_static::{__Deref, lazy_static};
use log::info;
use serde_json::json;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(test)]
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

lazy_static! {
    static ref SETTINGS_MAP: HashMap<&'static str, &'static str> = {
        let mut map = HashMap::new();
        map.insert("wifi", "/etc/wpa_supplicant/wpa_supplicant-wlan0.conf");
        map
    };
}

pub fn get_direct_methods(tx_app2client: Arc<Mutex<Sender<Message>>>) -> Option<DirectMethodMap> {
    let mut methods = DirectMethodMap::new();

    methods.insert(
        String::from("factory_reset"),
        IotHubClient::make_direct_method(move |in_json| {
            reset_to_factory_settings(in_json, Arc::clone(&tx_app2client))
        }),
    );

    methods.insert(String::from("user_consent"), Box::new(user_consent));

    methods.insert(String::from("reboot"), Box::new(reboot));

    Some(methods)
}

pub fn reset_to_factory_settings(
    in_json: serde_json::Value,
    tx: Arc<Mutex<Sender<Message>>>,
) -> Result<Option<serde_json::Value>> {
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

            twin::report_factory_reset_status(tx, "in_progress")?;

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
            let file_path = format!("{}/{}/user_consent.json", CONSENT_DIR_PATH, component);

            let mut file = OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open(&file_path)?;
            let content = serde_json::to_string_pretty(&json!({ "consent": version }))?;
            file.write(content.as_bytes())?;
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

#[test]
fn factory_reset_test() {
    let (tx_app2client, _) = mpsc::channel();
    let tx: Arc<Mutex<Sender<Message>>> = Arc::new(Mutex::new(tx_app2client));
    assert!(reset_to_factory_settings(
        json!({
            "type": 1,
            "restore_settings": ["wifi"]
        }),
        Arc::clone(&tx)
    )
    .unwrap_err()
    .to_string()
    .starts_with("No such file or directory"));

    assert!(reset_to_factory_settings(
        json!({
            "type": 1,
        }),
        Arc::clone(&tx)
    )
    .unwrap_err()
    .to_string()
    .starts_with("No such file or directory"));

    assert_eq!(
        reset_to_factory_settings(
            json!({
                "restore_settings": ["wifi"]
            }),
            Arc::clone(&tx)
        )
        .unwrap_err()
        .to_string(),
        "reset type missing or not supported"
    );

    assert_eq!(
        reset_to_factory_settings(
            json!({
                "type": 1,
                "restore_settings": ["unknown"]
            }),
            Arc::clone(&tx)
        )
        .unwrap_err()
        .to_string(),
        "unknown restore setting received"
    );
}

#[test]
fn user_consent_test() {
    let component = "swupdate";

    assert!(user_consent(json!({component: "1.0.0"}))
        .unwrap_err()
        .to_string()
        .starts_with("No such file or directory"));

    assert_eq!(
        user_consent(json!({})).unwrap_err().to_string(),
        "unexpected parameter format"
    );

    assert_eq!(
        user_consent(json!({component: "1.0.0", "another_component": "1.2.3"}))
            .unwrap_err()
            .to_string(),
        "unexpected parameter format"
    );
}
