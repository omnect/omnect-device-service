pub mod client;
pub mod direct_methods;
pub mod message;
#[cfg(feature = "systemd")]
pub mod systemd;
pub mod twin;
use azure_iot_sdk::client::*;
use client::{Client, Message};
use default_env::default_env;
use log::error;
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use std::sync::{mpsc, Arc, Mutex, Once};
use std::{path::Path, time::Duration};

static INIT: Once = Once::new();

pub static CONSENT_DIR_PATH: &'static str = default_env!("CONSENT_DIR_PATH", "/etc/ics_dm/consent");

const WATCHER_DELAY: u64 = 2;
const RX_CLIENT2APP_TIMEOUT: u64 = 1;


#[tokio::main]
pub async fn run() -> Result<(), IotError> {
    let mut client = Client::new();
    let (tx_client2app, rx_client2app) = mpsc::channel();
    let (tx_app2client, rx_app2client) = mpsc::channel();
    let (tx_file2app, rx_file2app) = mpsc::channel();
    let tx_app2client = Arc::new(Mutex::new(tx_app2client));
    let methods = direct_methods::get_direct_methods(Arc::clone(&tx_app2client));
    let mut debouncer =
        new_debouncer(Duration::from_secs(WATCHER_DELAY), None, tx_file2app).unwrap();
    let request_consent_path = format!("{}/request_consent.json", CONSENT_DIR_PATH);
    let history_consent_path = format!("{}/history_consent.json", CONSENT_DIR_PATH);

    debouncer
        .watcher()
        .watch(Path::new(&request_consent_path), RecursiveMode::Recursive)?;

    debouncer
        .watcher()
        .watch(Path::new(&history_consent_path), RecursiveMode::Recursive)?;

    client.run(None, methods, tx_client2app, rx_app2client);

    loop {
        match rx_client2app.recv_timeout(Duration::from_secs(RX_CLIENT2APP_TIMEOUT)) {
            Ok(Message::Authenticated) => {
                INIT.call_once(|| {
                    #[cfg(feature = "systemd")]
                    systemd::notify_ready();

                    if let Err(e) = twin::report_versions(Arc::clone(&tx_app2client)) {
                        error!("Couldn't report version: {}", e);
                    }

                    if let Err(e) = twin::report_general_consent(Arc::clone(&tx_app2client)) {
                        error!("Couldn't report general consent: {}", e);
                    }

                    if let Err(e) =
                        twin::report_user_consent(Arc::clone(&tx_app2client), &request_consent_path)
                    {
                        error!("Couldn't update {}: {}", request_consent_path, e);
                    }

                    if let Err(e) =
                        twin::report_user_consent(Arc::clone(&tx_app2client), &history_consent_path)
                    {
                        error!("Couldn't update {}: {}", history_consent_path, e);
                    }

                    if let Err(e) = twin::update_factory_reset_result(Arc::clone(&tx_app2client)) {
                        error!("Couldn't update factory reset result: {}", e);
                    }
                });
            }
            Ok(Message::Unauthenticated(reason)) => {
                if !matches!(reason, UnauthenticatedReason::ExpiredSasToken) {
                    return Err(IotError::from(format!(
                        "No connection. Reason: {:?}",
                        reason
                    )));
                }
            }
            Ok(Message::Desired(state, desired)) => {
                if let Err(e) = twin::update(state, desired, Arc::clone(&tx_app2client)) {
                    error!("Couldn't handle twin desired: {}", e);
                }
            }
            Ok(Message::C2D(msg)) => {
                message::update(msg, Arc::clone(&tx_app2client));
            }
            _ => {}
        }

        if let Ok(events) = rx_file2app.try_recv() {
            events.unwrap_or(vec![]).iter().for_each(|ev| {
                if let Some(path) = ev.path.to_str() {
                    if let Err(e) = twin::report_user_consent(Arc::clone(&tx_app2client), path) {
                        error!("Couldn't report user from {}: {}", path, e);
                    }
                }
            })
        }
    }
}
