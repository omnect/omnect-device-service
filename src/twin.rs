use crate::Message;
use azure_iot_sdk::client::*;
use time::{OffsetDateTime};
use time::format_description::well_known::Rfc3339;
use log::{debug, warn, error};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize, Debug)]
struct Status {
    status: String,
    date: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct FactoryResetStatus {
    factory_reset_status: Status,
}

pub fn update(
    _state: TwinUpdateState,
    _desired: serde_json::Value,
    _tx_app2client: Arc<Mutex<Sender<Message>>>,
) {
    warn!("Received unexpected desired twin properties");
}

pub fn report_factory_reset_result(tx_app2client: Arc<Mutex<Sender<Message>>>) {
    let output = Command::new("sh")
        .arg("-c")
        .arg("fw_printenv factory-reset-status")
        .output()
        .expect("failed to execute process");

    let get_env = String::from_utf8(output.stdout).unwrap();
    let split = get_env.split("=");
    let vec: Vec<&str> = split.collect();

    if vec.len() == 2 {
        let reset_state: String;
        match vec[1] {
            "0:0\n" => {
                debug!("successfully");
                reset_state = "successfully".to_string();
            }
            "1:-\n" => {
                error!("Unexpected factory reset type in result");
                reset_state = "type not supported".to_string();
            }
            _ => {
                error!("error");
                reset_state = "error".to_string();
            }
        }

        let state = FactoryResetStatus {
            factory_reset_status: Status {
                status: reset_state,
                date: OffsetDateTime::now_utc().format(&Rfc3339).unwrap().to_string(),
            },
        };
        let serialized_state = serde_json::to_string(&state).unwrap();
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&serialized_state).unwrap();

        debug!("Report factory reset: {:?}", map);

        tx_app2client
            .lock()
            .unwrap()
            .send(Message::Reported(serde_json::Value::Object(map)))
            .unwrap();

        Command::new("sh")
            .arg("-c")
            .arg("fw_setenv factory-reset-status")
            .output()
            .expect("failed to execute process");
    }
}
