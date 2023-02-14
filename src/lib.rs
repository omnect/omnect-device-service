pub mod client;
pub mod direct_methods;
pub mod message;
#[cfg(feature = "systemd")]
pub mod systemd;
pub mod twin;
use anyhow::{Context, Result};
use azure_iot_sdk::client::*;
use client::{Client, Message};
use default_env::default_env;
use log::error;
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use std::sync::{mpsc, Arc, Mutex, Once};
use std::{path::Path, time::Duration};
use twin::{ReportProperty, TWIN};

static INIT: Once = Once::new();

pub static CONSENT_DIR_PATH: &'static str = default_env!("CONSENT_DIR_PATH", "/etc/omnect/consent");

const WATCHER_DELAY: u64 = 2;
const RX_CLIENT2APP_TIMEOUT: u64 = 1;

#[tokio::main]
pub async fn run() -> Result<()> {
    let mut client = Client::new();
    let (tx_client2app, rx_client2app) = mpsc::channel();
    let (tx_app2client, rx_app2client) = mpsc::channel();
    let (tx_file2app, rx_file2app) = mpsc::channel();
    let tx_app2client = Arc::new(Mutex::new(tx_app2client));
    let methods = direct_methods::get_direct_methods();
    let mut debouncer =
        new_debouncer(Duration::from_secs(WATCHER_DELAY), None, tx_file2app).unwrap();
    let request_consent_path = format!("{}/request_consent.json", CONSENT_DIR_PATH);
    let history_consent_path = format!("{}/history_consent.json", CONSENT_DIR_PATH);

    TWIN.lock().unwrap().set_sender(Arc::clone(&tx_app2client));

    debouncer
        .watcher()
        .watch(Path::new(&request_consent_path), RecursiveMode::Recursive)
        .context("debouncer request_consent_path")?;

    debouncer
        .watcher()
        .watch(Path::new(&history_consent_path), RecursiveMode::Recursive)
        .context("debouncer history_consent_path")?;

    client.run(None, methods, tx_client2app, rx_app2client);

    loop {
        match rx_client2app.recv_timeout(Duration::from_secs(RX_CLIENT2APP_TIMEOUT)) {
            Ok(Message::Authenticated) => {
                INIT.call_once(|| {
                    #[cfg(feature = "systemd")]
                    systemd::notify_ready();

                    vec![
                        ReportProperty::Versions,
                        ReportProperty::GeneralConsent,
                        ReportProperty::UserConsent(&request_consent_path),
                        ReportProperty::UserConsent(&history_consent_path),
                        ReportProperty::FactoryResetResult,
                        ReportProperty::NetworkStatus,
                    ]
                    .iter()
                    .for_each(|p| {
                        TWIN.lock()
                            .unwrap()
                            .report(p)
                            .unwrap_or_else(|e| error!("{:#?}", e))
                    });
                });
            }
            Ok(Message::Unauthenticated(reason)) => {
                anyhow::ensure!(
                    matches!(reason, UnauthenticatedReason::ExpiredSasToken),
                    "No connection. Reason: {:?}",
                    reason
                );
            }
            Ok(Message::Desired(state, desired)) => {
                TWIN.lock()
                    .unwrap()
                    .update(state, desired)
                    .unwrap_or_else(|e| error!("{:#?}", e));
            }
            Ok(Message::C2D(msg)) => {
                message::update(msg, Arc::clone(&tx_app2client));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("iot channel unexpectedly closed by client");
            }
            _ => {}
        }

        if let Ok(events) = rx_file2app.try_recv() {
            events.unwrap_or(vec![]).iter().for_each(|ev| {
                if let Some(path) = ev.path.to_str() {
                    TWIN.lock()
                        .unwrap()
                        .report(&ReportProperty::UserConsent(path))
                        .unwrap_or_else(|e| error!("{:#?}", e));
                }
            })
        }
    }
}
