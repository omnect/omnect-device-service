#[cfg(not(any(feature = "device_twin", feature = "module_twin")))]
compile_error!(
    "Either feature \"device_twin\" xor \"module_twin\" must be enabled for this crate."
);

#[cfg(all(feature = "device_twin", feature = "module_twin"))]
compile_error!(
    "Either feature \"device_twin\" xor \"module_twin\" must be enabled for this crate."
);

#[cfg(feature = "device_twin")]
type TwinType = DeviceTwin;

#[cfg(feature = "module_twin")]
type TwinType = ModuleTwin;

pub mod client;
pub mod direct_methods;
pub mod message;
#[cfg(feature = "systemd")]
pub mod systemd;
pub mod twin;
use azure_iot_sdk::twin::*;
use client::{Client, Message};
use log::debug;
use std::error::Error;
use std::sync::{mpsc, Arc, Mutex};

pub fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut client = Client::new();
    let (tx_client2app, rx_client2app) = mpsc::channel();
    let (tx_app2client, rx_app2client) = mpsc::channel();
    let tx_app2client = Arc::new(Mutex::new(tx_app2client));
    let methods = direct_methods::get_direct_methods(Arc::clone(&tx_app2client));

    client.run::<TwinType>(None, methods, tx_client2app, rx_app2client);

    for msg in rx_client2app {
        match msg {
            Message::Authenticated => {
                #[cfg(feature = "systemd")]
                systemd::notify_ready();
                twin::report_factory_reset_result(Arc::clone(&tx_app2client))?;
            }
            Message::Unauthenticated(reason) => {
                client.stop().unwrap();
                return Err(Box::<dyn Error + Send + Sync>::from(format!(
                    "No connection. Reason: {:?}",
                    reason
                )));
            }
            Message::Desired(state, desired) => {
                twin::update(state, desired, Arc::clone(&tx_app2client));
            }
            Message::C2D(msg) => {
                message::update(msg, Arc::clone(&tx_app2client));
            }
            _ => debug!("Application received unhandled message"),
        }
    }

    client.stop()
}
