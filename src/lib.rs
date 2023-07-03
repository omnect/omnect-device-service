pub mod client;
#[cfg(feature = "bootloader_grub")]
pub mod grub_env;
pub mod systemd;
pub mod twin;
#[cfg(feature = "bootloader_uboot")]
pub mod uboot_env;
pub mod update_validation;

use anyhow::Result;
use azure_iot_sdk::client::*;
use client::{Client, Message};
use futures_executor::block_on;
use log::error;
use std::sync::{mpsc, Once};
use std::time::Duration;
#[cfg(test)]
mod test_util;

static INIT: Once = Once::new();

#[tokio::main]
pub async fn run() -> Result<()> {
    const RX_TIMEOUT_SECS: u64 = 1;
    let mut client = Client::new();
    let (tx_client2app, rx_client2app) = mpsc::channel();
    let (tx_app2client, rx_app2client) = mpsc::channel();
    let twin = twin::get_or_init(Some(&tx_app2client));

    client.run(None, twin.direct_methods(), tx_client2app, rx_app2client);

    loop {
        match rx_client2app.recv_timeout(Duration::from_secs(RX_TIMEOUT_SECS)) {
            Ok(Message::Authenticated) => {
                let mut update_validated = Ok(());

                INIT.call_once(|| {
                    /*
                     * the update validation test "wait_for_system_running" enforces that
                     * omnect-device-service already notified its own success
                     */
                    systemd::notify_ready();

                    update_validated = block_on(async { update_validation::check().await });

                    twin::get_or_init(None)
                        .exec_mut(|twin| twin.init_features())
                        .unwrap();
                });

                update_validated?;
            }
            Ok(Message::Unauthenticated(reason)) => {
                anyhow::ensure!(
                    matches!(reason, UnauthenticatedReason::ExpiredSasToken),
                    "No connection. Reason: {:?}",
                    reason
                );
            }
            Ok(Message::Desired(state, desired)) => {
                twin::get_or_init(None)
                    .exec_mut(|twin| twin.update(state, desired.to_owned()))
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
    }
}
