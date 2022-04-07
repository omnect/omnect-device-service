use crate::Message;
use azure_iot_sdk::client::*;
use log::{debug, warn};
use serde_json::json;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub fn update(
    state: TwinUpdateState,
    desired: serde_json::Value,
    tx_app2client: Arc<Mutex<Sender<Message>>>,
) {
    let status = match state {
        TwinUpdateState::Partial => match &desired["general_consent"].as_array() {
            Some(result) => Ok(json!({ "general_consent": result })),
            _ => Err(json!({ "general_consent": null })),
        },
        TwinUpdateState::Complete => match &desired["desired"]["general_consent"].as_array() {
            Some(result) => Ok(json!({ "general_consent": result })),
            _ => Err(json!({ "general_consent": null })),
        },
    };
    let file_handle = OpenOptions::new()
        .write(true)
        .create(false)
        .open("/etc/consent/consent_conf.json");

    match &status {
        Ok(report) => match file_handle {
            Ok(mut file) => {
                let content = serde_json::to_string_pretty(&report).unwrap();
                file.set_len(0).unwrap();
                file.write(content.as_bytes()).unwrap();
                file.write("\n".as_bytes()).unwrap();
            }
            _ => debug!("write to /etc/consent/consent_conf.json not possible"),
        },
        Err(_report) => match file_handle {
            Ok(file) => {
                file.set_len(0).unwrap();
            }
            _ => debug!("write to /etc/consent/consent_conf.json not possible"),
        },
    }

    match status {
        Ok(report) | Err(report) => {
            tx_app2client
                .lock()
                .unwrap()
                .send(Message::Reported(report))
                .unwrap();
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

pub fn report_user_consent(
    tx_app2client: Arc<Mutex<Sender<Message>>>,
    report_consent_file: PathBuf,
) -> Result<(), IotError> {
    debug!("report_user_consent_file: {:?}", report_consent_file);

    let data = fs::read_to_string(report_consent_file).expect("Unable to read file");
    let json: serde_json::Value = serde_json::from_str(&data).expect("JSON was not well-formatted");

    tx_app2client
        .lock()
        .unwrap()
        .send(Message::Reported(json))?;

    Ok(())
}
