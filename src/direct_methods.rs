use crate::twin;
use crate::Message;
use crate::CONSENT_DIR_PATH;
use azure_iot_sdk::client::*;
use lazy_static::__Deref;
use lazy_static::lazy_static;
use serde_json::json;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

lazy_static! {
    static ref SETTINGS_MAP: HashMap<&'static str, &'static str> = {
        let mut map = HashMap::new();
        map.insert("wifi", "/path/to/wpa_supplicant.conf");
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

    Some(methods)
}

pub fn reset_to_factory_settings(
    in_json: serde_json::Value,
    tx: Arc<Mutex<Sender<Message>>>,
) -> Result<Option<serde_json::Value>, IotError> {
    let restore_paths = match &in_json["restore_settings"].as_str() {
        Some(settings) => {
            let mut settings: Vec<&str> = serde_json::from_str(settings)?;
            let mut paths = vec![];

            settings.sort();
            settings.dedup();

            while let Some(s) = settings.pop() {
                if SETTINGS_MAP.contains_key(s) {
                    paths.push(SETTINGS_MAP.get(s).unwrap().deref());
                } else {
                    return Ok(Some(json!("unknown restore setting received")));
                }
            }

            paths
        }
        _ => vec![],
    };

    match &in_json["type"].as_str() {
        Some(reset_type) => {
            match OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open("/run/factory-reset/systemd-trigger")
            {
                Ok(mut file) => {
                    file.write(reset_type.as_bytes())?;

                    for p in restore_paths {
                        file.write(p.as_bytes())?;
                    }

                    twin::report_factory_reset_status(tx, "in_progress")?;
                    Ok(Some(json!("Ok")))
                }
                _ => Ok(Some(json!(
                    "write to /run/factory-reset/systemd-trigger not possible"
                ))),
            }
        }
        _ => Ok(Some(json!("param not supported"))),
    }
}

pub fn user_consent(in_json: serde_json::Value) -> Result<Option<serde_json::Value>, IotError> {
    match serde_json::from_value::<serde_json::Map<String, serde_json::Value>>(in_json) {
        Ok(map) if map.len() == 1 && map.values().next().unwrap().is_string() => {
            let (component, version) = map.iter().next().unwrap();
            let file_path = format!("{}/{}/user_consent.json", CONSENT_DIR_PATH, component);

            match OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open(&file_path)
            {
                Ok(mut file) => {
                    let content =
                        serde_json::to_string_pretty(&json!({ "consent": version })).unwrap();
                    file.write(content.as_bytes()).unwrap();
                    Ok(Some(json!("Ok")))
                }
                _ => Ok(Some(json!(format!("couldn't write to {}", file_path)))),
            }
        }
        _ => Ok(Some(json!("unexpected parameter format"))),
    }
}

#[test]
fn user_consent_test() {
    let component = "swupdate";
    let file_path = format!("{}/{}/user_consent.json", CONSENT_DIR_PATH, component);

    assert_eq!(
        user_consent(json!({component: "1.0.0"})).unwrap(),
        Some(json!(format!("couldn't write to {}", file_path)))
    );

    assert_eq!(
        user_consent(json!({})).unwrap(),
        Some(json!(format!("unexpected parameter format")))
    );

    assert_eq!(
        user_consent(json!({component: "1.0.0", "another_component": "1.2.3"})).unwrap(),
        Some(json!(format!("unexpected parameter format")))
    );
}
