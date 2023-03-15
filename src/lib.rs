pub mod client;
#[cfg(feature = "systemd")]
pub mod systemd;
pub mod twin;
pub mod update_validation;
use anyhow::{Context, Result};
use azure_iot_sdk::client::*;
use client::{Client, Message};
use log::error;
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use std::path::Path;
use std::sync::{mpsc, Once};
use std::time::Duration;
use twin::ReportProperty;
#[cfg(test)]
mod test_util;

static INIT: Once = Once::new();

#[macro_export]
macro_rules! consent_path {
    () => {{
        static CONSENT_DIR_PATH_DEFAULT: &'static str = "/etc/omnect/consent";
        std::env::var("CONSENT_DIR_PATH").unwrap_or(CONSENT_DIR_PATH_DEFAULT.to_string())
    }};
}

const WATCHER_DELAY: u64 = 2;
const RX_CLIENT2APP_TIMEOUT: u64 = 1;

fn report_states(request_consent_path: &str, history_consent_path: &str) {
    vec![
        ReportProperty::Versions,
        ReportProperty::GeneralConsent,
        ReportProperty::UserConsent(request_consent_path),
        ReportProperty::UserConsent(history_consent_path),
        ReportProperty::FactoryResetResult,
        ReportProperty::SshStatus,
    ]
    .iter()
    .for_each(|p| {
        twin::get_or_init(None)
            .report(p)
            .unwrap_or_else(|e| error!("twin report: {:#?}", e))
    });
}

#[tokio::main]
pub async fn run() -> Result<()> {
    let mut client = Client::new();
    let (tx_client2app, rx_client2app) = mpsc::channel();
    let (tx_app2client, rx_app2client) = mpsc::channel();
    let (tx_file2app, rx_file2app) = mpsc::channel();
    let mut debouncer =
        new_debouncer(Duration::from_secs(WATCHER_DELAY), None, tx_file2app).unwrap();
    let request_consent_path = format!("{}/request_consent.json", consent_path!());
    let history_consent_path = format!("{}/history_consent.json", consent_path!());
    let twin = twin::get_or_init(Some(&tx_app2client));

    debouncer
        .watcher()
        .watch(Path::new(&request_consent_path), RecursiveMode::Recursive)
        .context("debouncer request_consent_path")?;

    debouncer
        .watcher()
        .watch(Path::new(&history_consent_path), RecursiveMode::Recursive)
        .context("debouncer history_consent_path")?;

    client.run(
        None,
        twin.get_direct_methods(),
        tx_client2app,
        rx_app2client,
    );

    loop {
        match rx_client2app.recv_timeout(Duration::from_secs(RX_CLIENT2APP_TIMEOUT)) {
            Ok(Message::Authenticated) => {
                INIT.call_once(|| {
                    update_validation::check_update().unwrap();

                    #[cfg(feature = "systemd")]
                    systemd::notify_ready();

                    report_states(&request_consent_path, &history_consent_path);
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
                twin.update(state, desired)
                    .unwrap_or_else(|e| error!("twin update desired properties: {:#?}", e));
            }
            Ok(Message::C2D(msg)) => {
                twin.cloud_message(msg);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("iot channel unexpectedly closed by client");
            }
            _ => {}
        }

        if let Ok(events) = rx_file2app.try_recv() {
            events.unwrap_or_default().iter().for_each(|ev| {
                if let Some(path) = ev.path.to_str() {
                    twin.report(&ReportProperty::UserConsent(path))
                        .unwrap_or_else(|e| error!("twin report user consent: {:#?}", e));
                }
            })
        }
    }
}
