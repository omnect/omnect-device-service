use azure_iot_sdk::client::*;
use futures_executor::block_on;
use log::{error, info, warn};
use std::sync::{mpsc::Receiver, mpsc::Sender, Arc, Mutex};
use std::time;
use tokio::task::JoinHandle;

#[cfg(feature = "systemd")]
use crate::systemd::WatchdogHandler;

#[derive(Debug)]
pub enum Message {
    Desired(TwinUpdateState, serde_json::Value),
    Reported(serde_json::Value),
    D2C(IotMessage),
    C2D(IotMessage),
    Authenticated,
    Unauthenticated(UnauthenticatedReason),
    Terminate,
}

struct ClientEventHandler {
    direct_methods: Option<DirectMethodMap>,
    tx: Sender<Message>,
}

impl EventHandler for ClientEventHandler {
    fn handle_connection_status(&self, auth_status: AuthenticationStatus) {
        info!("new AuthenticationStatus: {:?}", auth_status);

        let res = match auth_status {
            AuthenticationStatus::Authenticated => self.tx.send(Message::Authenticated),
            AuthenticationStatus::Unauthenticated(reason) => {
                self.tx.send(Message::Unauthenticated(reason))
            }
        };

        if let Err(e) = res {
            warn!(
                "Couldn't send AuthenticationStatus since the receiver was closed: {}",
                e.to_string()
            )
        }
    }

    fn handle_c2d_message(&self, message: IotMessage) -> Result<(), IotError> {
        self.tx.send(Message::C2D(message))?;
        Ok(())
    }

    fn get_c2d_message_property_keys(&self) -> Vec<&'static str> {
        vec!["p1", "p2"]
    }

    fn handle_twin_desired(
        &self,
        state: TwinUpdateState,
        desired: serde_json::Value,
    ) -> Result<(), IotError> {
        self.tx.send(Message::Desired(state, desired))?;

        Ok(())
    }

    fn get_direct_methods(&self) -> Option<&DirectMethodMap> {
        self.direct_methods.as_ref()
    }
}

pub struct Client {
    thread: Option<JoinHandle<Result<(), IotError>>>,
    run: Arc<Mutex<bool>>,
}

impl Client {
    pub fn new() -> Self {
        Client {
            thread: None,
            run: Arc::new(Mutex::new(false)),
        }
    }

    pub fn run(
        &mut self,
        connection_string: Option<&'static str>,
        direct_methods: Option<DirectMethodMap>,
        tx: Sender<Message>,
        rx: Receiver<Message>,
    ) {
        *self.run.lock().unwrap() = true;

        let running = Arc::clone(&self.run);

        self.thread = Some(tokio::task::spawn_blocking(move || {
            let hundred_millis = time::Duration::from_millis(100);
            let event_handler = ClientEventHandler { direct_methods, tx };

            #[cfg(feature = "systemd")]
            let mut wdt = WatchdogHandler::default();

            #[cfg(feature = "systemd")]
            wdt.init()?;

            let mut client = match IotHubClient::get_client_type() {
                _ if connection_string.is_some() => {
                    IotHubClient::from_connection_string(connection_string.unwrap(), event_handler)?
                }
                ClientType::Device | ClientType::Module => {
                    IotHubClient::from_identity_service(event_handler)?
                }
                ClientType::Edge => IotHubClient::from_edge_environment(event_handler)?,
            };

            while *running.lock().unwrap() {
                match rx.recv_timeout(hundred_millis) {
                    Ok(Message::Reported(reported)) => client.send_reported_state(reported)?,
                    Ok(Message::D2C(telemetry)) => {
                        client.send_d2c_message(telemetry).map(|_| ())?
                    }
                    Ok(Message::Terminate) => return Ok(()),
                    Ok(_) => info!("Client received unhandled message"),
                    Err(_) => (),
                };

                client.do_work();

                #[cfg(feature = "systemd")]
                wdt.notify()?;
            }

            Ok(())
        }));
    }

    pub fn stop(&mut self) -> Result<(), IotError> {
        if self.thread.is_some() {
            return block_on(async {
                *self.run.lock().unwrap() = false;
                self.thread.take().as_mut().unwrap().await?
            });
        }

        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            error!("Client thread returned with: {}", e);
        }
    }
}
