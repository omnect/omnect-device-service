use crate::{Message};
use azure_iot_sdk::client::*;
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Deserialize, Debug, Serialize)]
struct Factory {
    reset: String,
}

pub fn get_direct_methods(
    _tx_app2client: Arc<Mutex<Sender<Message>>>,
) -> Option<HashMap<String, DirectMethod>> {
    let mut methods: HashMap<String, DirectMethod> = HashMap::new();

    methods.insert(String::from("factory"), Box::new(reset_to_factory_settings));

    Some(methods)
}

pub fn reset_to_factory_settings(
    in_json: serde_json::Value,
) -> Result<Option<serde_json::Value>, Box<dyn Error + Send + Sync>> {
    let content: Factory;
    let result = match serde_json::from_value(in_json) {
        Ok(param) => {
            content = param;
            debug!("direct method called with: {:?}", content.reset);
            write_to_file(content.reset)
        }
        _ => {
            "param not supported".to_string()
        }
    };

    Ok(Some(serde_json::to_value(result).unwrap()))
}

fn write_to_file(reset_type: String) -> String {
    match OpenOptions::new()
        .write(true)
        .create(false)
        .open("/run/factory-reset/systemd-trigger")
    {
        Ok(mut file) => {
            let content = format!("{}\n", reset_type.as_str());

            file.write(content.as_bytes()).unwrap();
            "Ok".to_string()
        }
        _ => {
            "file not exists".to_string()
        }
    }
}
