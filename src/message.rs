use crate::Message;
use azure_iot_sdk::client::IotMessage;
use log::warn;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub fn update(msg: IotMessage, _tx_app2client: Arc<Mutex<Sender<Message>>>) {
    warn!(
        "Received unexpected C2D message with \n body: {:?}\n properties: {:?} \n system properties: {:?}",
        std::str::from_utf8(&msg.body).unwrap(),
        msg.properties,
        msg.system_properties
    );
}
