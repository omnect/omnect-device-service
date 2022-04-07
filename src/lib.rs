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
use notify::{watcher, RecursiveMode, Watcher};
use std::sync::mpsc::channel;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub fn run() -> Result<(), IotError> {
    let mut client = Client::new();
    let (tx_client2app, rx_client2app) = mpsc::channel();
    let (tx_app2client, rx_app2client) = mpsc::channel();
    let tx_app2client = Arc::new(Mutex::new(tx_app2client));
    let methods = direct_methods::get_direct_methods(Arc::clone(&tx_app2client));
    let twin_type = TwinType::Module;
    let (tx, rx) = channel();

    // Create a watcher object, delivering debounced events.
    // The notification back-end is selected based on the platform.
    let mut watcher = watcher(tx, Duration::from_secs(10)).unwrap();

    // Add a path to be watched. All files and directories at that path and
    // below will be monitored for changes.
    watcher
        .watch(
            "/etc/consent/request_consent.json",
            RecursiveMode::Recursive,
        )
        .unwrap();
    watcher
        .watch("/etc/consent/agreed_consent.json", RecursiveMode::Recursive)
        .unwrap();

    client.run(twin_type, None, methods, tx_client2app, rx_app2client);

    loop {
        match rx_client2app.recv_timeout(Duration::from_secs(1)) {
            Ok(Message::Authenticated) => {
                #[cfg(feature = "systemd")]
                systemd::notify_ready();
                twin::report_factory_reset_result(Arc::clone(&tx_app2client))?;
            }
            Ok(Message::Unauthenticated(reason)) => {
                client.stop().unwrap();
                return Err(IotError::from(format!(
                    "No connection. Reason: {:?}",
                    reason
                )));
            }
            Ok(Message::Desired(state, desired)) => {
                twin::update(state, desired, Arc::clone(&tx_app2client));
            }
            Ok(Message::C2D(msg)) => {
                message::update(msg, Arc::clone(&tx_app2client));
            }
            _ => {}
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(event) => match event {
                notify::DebouncedEvent::Write(file_report) => {
                    twin::report_user_consent(Arc::clone(&tx_app2client), file_report)?
                }
                _ => println!("{:?}", event),
            },
            Err(_e) => {}
        }
    }
}
