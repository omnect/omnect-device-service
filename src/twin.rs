use crate::Message;
use azure_iot_sdk::{client::*, IotError};
use log::{warn, debug};
use serde_json::json;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub fn update(
    _state: TwinUpdateState,
    _desired: serde_json::Value,
    _tx_app2client: Arc<Mutex<Sender<Message>>>,
) {
    warn!("Received unexpected desired twin properties");
}

pub fn report_factory_reset_result(
    tx_app2client: Arc<Mutex<Sender<Message>>>,
) -> Result<(), IotError> {
    let output = Command::new("sh")
        .arg("-c")
        .arg("fw_printenv factory-reset-status")
        .output()?;

    let status = String::from_utf8(output.stdout)?;
    let vec: Vec<&str> = status.split("=").collect();

    let status = match vec[..] {
        ["factory-reset-status", "0:0\n"] => Ok(("succeeded",true)),
        ["factory-reset-status", "1:-\n"] => Ok(("unexpected factory reset type",true)),
        ["factory-reset-status", "\n"] => Ok(("normal boot without factory reset",false)),
        ["factory-reset-status", _] => Ok(("failed",true)),
        _ => Err(("unexpected factory reset result format",false)),
    };

    match status {
        Ok((update_twin,true)) => {
            tx_app2client
            .lock()
            .unwrap()
            .send(Message::Reported(json!({
                "factory_reset_status": {
                    "status": update_twin,
                    "date": OffsetDateTime::now_utc().format(&Rfc3339)?.to_string(),
                }
            })))?;
            Command::new("sh")
            .arg("-c")
            .arg("fw_setenv factory-reset-status")
            .output()?;
        },
        Ok((update_twin,false)) => {
            debug!("{}",update_twin);
        },
        Err((update_twin,_)) => {
            warn!("factory reset result: {}",update_twin);
        },
    };

    Ok(())
}
