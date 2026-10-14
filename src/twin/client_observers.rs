use crate::twin::{IotHubClient, IotHubClientBuilder};
use anyhow::anyhow;
use azure_iot_sdk::client::{AuthenticationStatus, DirectMethod, TwinUpdate};
use log::{debug, error, warn};
use tokio::{runtime::Handle, select, sync::mpsc, task::JoinHandle};

const OBSERVER_CHANNEL_SIZE: usize = 100;

/// Receivers of the callbacks of one iot hub client.
pub struct ClientObservers {
    pub rx_connection_status: mpsc::Receiver<AuthenticationStatus>,
    pub rx_twin_desired: mpsc::Receiver<TwinUpdate>,
    pub rx_direct_method: mpsc::Receiver<DirectMethod>,
}

impl ClientObservers {
    /// The builder holds the only senders, so the channels close when the
    /// client built from it is dropped.
    pub fn builder() -> (IotHubClientBuilder, Self) {
        let (tx_connection_status, rx_connection_status) = mpsc::channel(OBSERVER_CHANNEL_SIZE);
        let (tx_twin_desired, rx_twin_desired) = mpsc::channel(OBSERVER_CHANNEL_SIZE);
        let (tx_direct_method, rx_direct_method) = mpsc::channel(OBSERVER_CHANNEL_SIZE);

        let builder = IotHubClient::builder()
            .observe_connection_state(tx_connection_status)
            .observe_desired_properties(tx_twin_desired)
            .observe_direct_methods(tx_direct_method);

        (
            builder,
            Self {
                rx_connection_status,
                rx_twin_desired,
                rx_direct_method,
            },
        )
    }

    /// Must start before the client is destroyed: the SDK joins its callback
    /// thread on destroy, and a callback blocks until its message is received
    /// and a direct method is answered. Runs on a blocking thread, so it also
    /// runs while destroy blocks a runtime worker thread.
    pub fn drain(mut self) -> JoinHandle<()> {
        let runtime = Handle::current();

        tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                loop {
                    select! {
                        Some(dm) = self.rx_direct_method.recv() => {
                            warn!("reject direct method {} of the old iothub client", dm.name);
                            if dm.responder.send(Err(anyhow!("iothub client reset"))).is_err() {
                                error!("direct method response receiver dropped");
                            }
                        },
                        Some(status) = self.rx_connection_status.recv() => {
                            debug!("drop connection status {status:?} of the old iothub client");
                        },
                        Some(_) = self.rx_twin_desired.recv() => {
                            debug!("drop desired properties of the old iothub client");
                        },
                        else => break,
                    }
                }
            })
        })
    }
}
