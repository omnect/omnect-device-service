use azure_iot_sdk::client::*;
use azure_iot_sdk::message::*;
use log::debug;
use std::collections::HashMap;
use std::error::Error;
use std::sync::{mpsc::Receiver, mpsc::Sender, Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time;

#[derive(Debug)]
pub enum Message {
    Desired(TwinUpdateState, serde_json::Value),
    Reported(serde_json::Value),
    Terminate,
}

struct IotModuleEventHandler {
    direct_methods: Option<HashMap<String, DirectMethod>>,
    tx: Sender<Message>,
}

impl EventHandler for IotModuleEventHandler {
    fn handle_message(&self, _message: IotHubMessage) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    fn handle_twin_desired(
        &self,
        state: TwinUpdateState,
        desired: serde_json::Value,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.tx.send(Message::Desired(state, desired))?;

        Ok(())
    }

    fn get_direct_methods(&self) -> Option<&HashMap<String, DirectMethod>> {
        self.direct_methods.as_ref()
    }
}

pub struct IotModuleTemplate {
    thread: Option<JoinHandle<Result<(), Box<dyn Error + Send + Sync + Send + Sync>>>>,
    run: Arc<Mutex<bool>>,
}

impl IotModuleTemplate {
    pub fn new() -> Self {
        IotModuleTemplate {
            thread: None,
            run: Arc::new(Mutex::new(false)),
        }
    }

    pub fn run(
        &mut self,
        connection_string: Option<&'static str>,
        direct_methods: Option<HashMap<String, DirectMethod>>,
        tx: Sender<Message>,
        rx: Receiver<Message>,
    ) {
        *self.run.lock().unwrap() = true;

        let running = Arc::clone(&self.run);

        self.thread = Some(thread::spawn(
            move || -> Result<(), Box<dyn Error + Send + Sync + Send + Sync>> {
                let hundred_millis = time::Duration::from_millis(100);
                let event_handler = IotModuleEventHandler { direct_methods, tx };

                let mut client = match connection_string {
                    Some(cs) => IotHubModuleClient::from_connection_string(cs, event_handler)?,
                    _ => IotHubModuleClient::from_identity_service(event_handler)?,
                };

                while *running.lock().unwrap() {
                    match rx.recv_timeout(hundred_millis) {
                        Ok(Message::Reported(reported)) => client.send_reported_state(reported)?,
                        Ok(Message::Terminate) => return Ok(()),
                        Ok(_) => debug!("Client received unhandled message"),
                        Err(_) => (),
                    };

                    client.do_work();
                }

                Ok(())
            },
        ));
    }
    pub fn stop(self) -> Result<(), Box<dyn Error + Send + Sync>> {
        *self.run.lock().unwrap() = false;

        self.thread.map_or(Ok(()), |t| t.join().unwrap())
    }

    pub fn make_direct_method<'a, F>(f: F) -> DirectMethod
    where
        F: Fn(serde_json::Value) -> Result<Option<serde_json::Value>, Box<dyn Error + Send + Sync>>
            + 'static
            + Send,
    {
        Box::new(f) as DirectMethod
    }
}
