use crate::Message;
use azure_iot_sdk::client::*;
use log::{debug, warn};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
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
    if let TwinUpdateState::Partial = _state {
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(_desired).unwrap();

        match map["user_consent"]["swupdate"].as_str() {
            Some(consent) => {
                match OpenOptions::new()
                    .write(true)
                    .create(false)
                    .open("/etc/ics_dm/user_consent_conf")
                {
                    Ok(mut file) => {
                        file.set_len(0).unwrap(); // delete file content
                        file.write("swupdate ".as_bytes()).unwrap();
                        file.write(consent.as_bytes()).unwrap();
                        file.write("\n".as_bytes()).unwrap();
                    }
                    _ => debug!("write to /etc/ics_dm/user_consent_conf not possible"),
                }
            }
            _ => {
                debug!("user consent for swupdate not available, clean consent conf file!");
                match OpenOptions::new()
                    .write(true)
                    .create(false)
                    .open("/etc/ics_dm/user_consent_conf")
                {
                    Ok(file) => {
                        file.set_len(0).unwrap(); // delete file content
                    }
                    _ => debug!("clear /etc/ics_dm/user_consent_conf not possible"),
                }
            }
        }
    }
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
        ["factory-reset-status", "0:0\n"] => Ok(("succeeded", true)),
        ["factory-reset-status", "1:-\n"] => Ok(("unexpected factory reset type", true)),
        ["factory-reset-status", "\n"] => Ok(("normal boot without factory reset", false)),
        ["factory-reset-status", _] => Ok(("failed", true)),
        _ => Err("unexpected factory reset result format"),
    };

    match status {
        Ok((update_twin, true)) => {
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
        }
        Ok((update_twin, false)) => {
            debug!("{}", update_twin);
        }
        Err(update_twin) => {
            warn!("factory reset result: {}", update_twin);
        }
    };

    Ok(())
}
