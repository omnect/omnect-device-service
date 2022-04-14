use crate::Message;
use crate::CONSENT_DIR_PATH;
use azure_iot_sdk::client::*;
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub fn get_direct_methods(_tx_app2client: Arc<Mutex<Sender<Message>>>) -> Option<DirectMethodMap> {
    let mut methods = DirectMethodMap::new();

    methods.insert(String::from("factory"), Box::new(reset_to_factory_settings));
    methods.insert(String::from("user_consent"), Box::new(user_consent));

    Some(methods)
}

pub fn reset_to_factory_settings(
    in_json: serde_json::Value,
) -> Result<Option<serde_json::Value>, IotError> {
    match &in_json["reset"].as_str() {
        Some(reset_type) => {
            match OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open("/run/factory-reset/systemd-trigger")
            {
                Ok(mut file) => {
                    file.write(reset_type.as_bytes())?;
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
            let file_path = format!("{CONSENT_DIR_PATH}/{component}/user_consent.json");

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
                _ => Ok(Some(json!(format!("couldn't write to {file_path}")))),
            }
        }
        _ => Ok(Some(json!("unexpected parameter format"))),
    }
}

#[test]
fn user_consent_test() {
    let component = "swupdate";
    let file_path = format!("{CONSENT_DIR_PATH}/{component}/user_consent.json");

    assert_eq!(
        user_consent(json!({component: "1.0.0"})).unwrap(),
        Some(json!(format!("couldn't write to {file_path}")))
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
