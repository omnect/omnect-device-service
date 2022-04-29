#[macro_use]
extern crate default_env;
#[cfg(not(any(feature = "device_twin", feature = "module_twin")))]
compile_error!(
    "Either feature \"device_twin\" xor \"module_twin\" must be enabled for this crate."
);

#[cfg(all(feature = "device_twin", feature = "module_twin"))]
compile_error!(
    "Either feature \"device_twin\" xor \"module_twin\" must be enabled for this crate."
);

pub mod client;
pub mod direct_methods;
pub mod message;
#[cfg(feature = "systemd")]
pub mod systemd;
pub mod twin;
use azure_iot_sdk::client::*;
use client::{Client, Message};
use log::error;
use notify::{RecursiveMode, Watcher};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

pub static CONSENT_DIR_PATH: &'static str = default_env!("CONSENT_DIR_PATH", "/etc/ics_dm/consent");

const WATCHER_DELAY: u64 = 2;
const RX_CLIENT2APP_TIMEOUT: u64 = 1;

pub fn run() -> Result<(), IotError> {
    let mut client = Client::new();
    let (tx_client2app, rx_client2app) = mpsc::channel();
    let (tx_app2client, rx_app2client) = mpsc::channel();
    let tx_app2client = Arc::new(Mutex::new(tx_app2client));
    let methods = direct_methods::get_direct_methods(Arc::clone(&tx_app2client));
    let twin_type = if cfg!(feature = "device_twin") {
        TwinType::Device
    } else {
        TwinType::Module
    };
    let (tx_file_watcher, rx_file_watcher) = mpsc::channel();
    let mut watcher = notify::watcher(tx_file_watcher, Duration::from_secs(WATCHER_DELAY))?;

    watcher.watch(
        format!("{}/request_consent.json", CONSENT_DIR_PATH),
        RecursiveMode::Recursive,
    )?;

    watcher.watch(
        format!("{}/history_consent.json", CONSENT_DIR_PATH),
        RecursiveMode::Recursive,
    )?;

    client.run(twin_type, None, methods, tx_client2app, rx_app2client);

    loop {
        match rx_client2app.try_recv() {
            Ok(Message::Authenticated) => {
                #[cfg(feature = "systemd")]
                systemd::notify_ready();
                twin::report_general_consent(Arc::clone(&tx_app2client))?;
                twin::update_factory_reset_result(Arc::clone(&tx_app2client))?;
            }
            Ok(Message::Unauthenticated(reason)) => {
                client.stop()?;
                return Err(IotError::from(format!(
                    "No connection. Reason: {:?}",
                    reason
                )));
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

        if let Ok(notify::DebouncedEvent::Write(file)) = rx_file_watcher.try_recv() {
            twin::report_user_consent(Arc::clone(&tx_app2client), file)?
        }

        thread::sleep(Duration::from_secs(RX_CLIENT2APP_TIMEOUT));
    }
}
