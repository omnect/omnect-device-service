use azure_iot_sdk::client::*;
use log::debug;
use std::sync::{mpsc::Receiver, mpsc::Sender, Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time;

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
        match auth_status {
            AuthenticationStatus::Authenticated => self.tx.send(Message::Authenticated).unwrap(),
            AuthenticationStatus::Unauthenticated(reason) => {
                self.tx.send(Message::Unauthenticated(reason)).unwrap()
            }
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
        twin_type: TwinType,
        connection_string: Option<&'static str>,
        direct_methods: Option<DirectMethodMap>,
        tx: Sender<Message>,
        rx: Receiver<Message>,
    ) {
        *self.run.lock().unwrap() = true;

        let running = Arc::clone(&self.run);

        self.thread = Some(thread::spawn(move || -> Result<(), IotError> {
            let hundred_millis = time::Duration::from_millis(100);
            let event_handler = ClientEventHandler { direct_methods, tx };

            let mut client = match connection_string {
                Some(cs) => IotHubClient::from_connection_string(twin_type, cs, event_handler)?,
                _ => IotHubClient::from_identity_service(twin_type, event_handler)?,
            };

            #[cfg(feature = "systemd")]
            let mut wdt = WatchdogHandler::default();

            #[cfg(feature = "systemd")]
            wdt.init()?;

            while *running.lock().unwrap() {
                match rx.recv_timeout(hundred_millis) {
                    Ok(Message::Reported(reported)) => client.send_reported_state(reported)?,
                    Ok(Message::D2C(telemetry)) => {
                        client.send_d2c_message(telemetry).map(|_| ())?
                    }
                    Ok(Message::Terminate) => return Ok(()),
                    Ok(_) => debug!("Client received unhandled message"),
                    Err(_) => (),
                };

                client.do_work();

                #[cfg(feature = "systemd")]
                wdt.notify()?;
            }

            Ok(())
        }));
    }
    pub fn stop(self) -> Result<(), IotError> {
        *self.run.lock().unwrap() = false;

        self.thread.map_or(Ok(()), |t| t.join().unwrap())
    }
}
