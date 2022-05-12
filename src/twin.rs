use crate::Message;
use crate::CONSENT_DIR_PATH;
use azure_iot_sdk::client::*;
use log::info;
use log::{debug, warn};
use serde_json::json;
use std::fs;
use std::fs::OpenOptions;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub fn update(
    state: TwinUpdateState,
    desired: serde_json::Value,
    tx_app2client: Arc<Mutex<Sender<Message>>>,
) -> Result<(), IotError> {
    desired_general_consent(state, desired, tx_app2client)
}

fn desired_general_consent(
    state: TwinUpdateState,
    desired: serde_json::Value,
    tx_app2client: Arc<Mutex<Sender<Message>>>,
) -> Result<(), IotError> {
    struct Guard {
        tx_app2client: Arc<Mutex<Sender<Message>>>,
        report_default: bool,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.report_default {
                self.tx_app2client
                    .lock()
                    .unwrap()
                    .send(Message::Reported(json!({ "general_consent": null })))
                    .unwrap()
            }
        }
    }

    let mut guard = Guard {
        tx_app2client: Arc::clone(&tx_app2client),
        report_default: true,
    };

    if let Some(consents) = match state {
        TwinUpdateState::Partial => desired["general_consent"].as_array(),
        TwinUpdateState::Complete => desired["desired"]["general_consent"].as_array(),
    } {
        let file = OpenOptions::new()
            .write(true)
            .create(false)
            .truncate(true)
            .open(format!("{}/consent_conf.json", CONSENT_DIR_PATH))?;

        serde_json::to_writer_pretty(file, &json!({ "general_consent": consents }))?;
    } else {
        info!("no general consent defined in desired properties");
    }

    report_general_consent(Arc::clone(&tx_app2client))?;

    guard.report_default = false;

    Ok(())
}

pub fn report_version(tx_app2client: Arc<Mutex<Sender<Message>>>) -> Result<(), IotError> {
    tx_app2client
        .lock()
        .unwrap()
        .send(Message::Reported(json!({
            "module-version": env!("CARGO_PKG_VERSION")
        })))?;

    Ok(())
}

pub fn report_general_consent(tx_app2client: Arc<Mutex<Sender<Message>>>) -> Result<(), IotError> {
    let file = OpenOptions::new()
        .read(true)
        .create(false)
        .open(format!("{}/consent_conf.json", CONSENT_DIR_PATH))?;

    tx_app2client
        .lock()
        .unwrap()
        .send(Message::Reported(serde_json::from_reader(file)?))?;

    Ok(())
}

pub fn report_user_consent(
    tx_app2client: Arc<Mutex<Sender<Message>>>,
    report_consent_file: &str,
) -> Result<(), IotError> {
    debug!("report_user_consent_file: {:?}", report_consent_file);

    let json: serde_json::Value =
        serde_json::from_str(fs::read_to_string(report_consent_file)?.as_str())?;

    tx_app2client
        .lock()
        .unwrap()
        .send(Message::Reported(json))?;

    Ok(())
}

pub fn report_factory_reset_status(
    tx_app2client: Arc<Mutex<Sender<Message>>>,
    status: &str,
) -> Result<(), IotError> {
    tx_app2client
        .lock()
        .unwrap()
        .send(Message::Reported(json!({
            "factory_reset_status": {
                "status": status,
                "date": OffsetDateTime::now_utc().format(&Rfc3339)?.to_string(),
            }
        })))?;

    Ok(())
}

pub fn update_factory_reset_result(
    tx_app2client: Arc<Mutex<Sender<Message>>>,
) -> Result<(), IotError> {
    if let Ok(output) = Command::new("sh")
        .arg("-c")
        .arg("fw_printenv factory-reset-status")
        .output()
    {
        let status = String::from_utf8(output.stdout)?;
        let vec: Vec<&str> = status.split("=").collect();

        let status = match vec[..] {
            ["factory-reset-status", "0:0\n"] => Ok(("succeeded", true)),
            ["factory-reset-status", "1:-\n"] => Ok(("unexpected factory reset type", true)),
            ["factory-reset-status", "2:-\n"] => Ok(("unexpected restore settings error", true)),
            ["factory-reset-status", "\n"] => Ok(("normal boot without factory reset", false)),
            ["factory-reset-status", _] => Ok(("failed", true)),
            _ => Err("unexpected factory reset result format"),
        };

        match status {
            Ok((update_twin, true)) => {
                report_factory_reset_status(tx_app2client, update_twin)?;
                Command::new("sh")
                    .arg("-c")
                    .arg("fw_setenv factory-reset-status")
                    .output()?;

                debug!("factory reset result: {}", update_twin);
            }
            Ok((update_twin, false)) => {
                debug!("factory reset result: {}", update_twin);
            }
            Err(update_twin) => {
                warn!("factory reset result: {}", update_twin);
            }
        };
    } else {
        warn!("fw_printenv command not supported");
    }

    Ok(())
}
