use crate::{Client, Message};
use azure_iot_sdk::client::*;
use azure_iot_sdk::message::*;
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
    tx_app2client: Arc<Mutex<Sender<Message>>>,
) -> Option<HashMap<String, DirectMethod>> {
    let mut methods = HashMap::new();
    methods.insert(
        String::from("closure_send_d2c_message"),
        Client::make_direct_method(move |_in_json| {
            let msg = IotMessage::builder()
                .set_body(
                    serde_json::to_vec(r#"{"my telemetry message": "hi from device"}"#).unwrap(),
                )
                .set_id(String::from("my msg id"))
                .set_correlation_id(String::from("my correleation id"))
                .set_property(
                    String::from("my property key"),
                    String::from("my property value"),
                )
                .set_output_queue(String::from("my output queue"))
                .build();

            tx_app2client
                .lock()
                .unwrap()
                .send(Message::D2C(msg))
                .unwrap();
            Ok(None)
        }),
    );

    methods.insert(String::from("factory"), Box::new(factory));

    Some(methods)
}

pub fn factory(
    in_json: serde_json::Value,
) -> Result<Option<serde_json::Value>, Box<dyn Error + Send + Sync>> {
    let content: Factory;
    let result;
    match serde_json::from_value(in_json) {
        Ok(param) => {
            content = param;
            debug!("direct method called with: {:?}", content.reset);
            result = write_to_file(content.reset);
        }
        _ => {
            result = "param not supported".to_string();
        }
    }

    Ok(Some(serde_json::to_value(result).unwrap()))
}

fn write_to_file(reset_type: String) -> String {
    let res;
    match OpenOptions::new()
        .write(true)
        .create(false)
        .open("/run/factory-reset")
    {
        Ok(mut file) => {
            let content = format!("{}\n", reset_type.as_str());

            file.write(content.as_bytes()).unwrap();
            res = "Ok".to_string();
        }
        _ => {
            res = "file not exists".to_string();
        }
    }
    res
}
