use crate::Message;
use azure_iot_sdk::{client::*, IotError};
use log::debug;
use serde_json::json;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub fn get_direct_methods(
    _tx_app2client: Arc<Mutex<Sender<Message>>>,
) -> Option<HashMap<String, DirectMethod>> {
    let mut methods: HashMap<String, DirectMethod> = HashMap::new();

    methods.insert(String::from("factory"), Box::new(reset_to_factory_settings));

    Some(methods)
}

pub fn reset_to_factory_settings(
    in_json: serde_json::Value,
) -> Result<Option<serde_json::Value>, IotError> {
    match &in_json["reset"].as_str() {
        Some(reset_type) => {
            debug!("direct method called with: {:?}", reset_type);
            match OpenOptions::new()
                .write(true)
                .create(false)
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
