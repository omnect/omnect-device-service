pub mod demo_portal_module;
use azure_iot_sdk::client::*;
use demo_portal_module::{IotModuleTemplate, Message};
use log::debug;
use serde_json::json;
use std::collections::HashMap;
use std::error::Error;
use std::sync::mpsc;

mod demo_portal;

pub fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let (tx_client2app, rx_client2app) = mpsc::channel();
    let (tx_app2client, rx_app2client) = mpsc::channel();
    // connect via identity servcie
    // let connection_string = None;
    // alternatively use connection string
    let connection_string = Some("HostName=iothub-ics-dev.azure-devices.net;DeviceId=joz-rust-test-02:42:ac:14:00:02;ModuleId=ics-dm-iot-module-rs;SharedAccessKey=vd7OI7bDGaeK+kFwGwBN4I5jJG6ufryLOdkC35GzS8o=");

    let mut methods = HashMap::<String, DirectMethod>::new();

    methods.insert(
        String::from("func_params_as_result"),
        Box::new(func_params_as_result),
    );

    methods.insert(String::from("factory"), Box::new(demo_portal::factory));

    let mut template = IotModuleTemplate::new();

    template.run(
        connection_string,
        Some(methods),
        tx_client2app,
        rx_app2client,
    );

    for msg in rx_client2app {
        match msg {
            _ => debug!("Application received unhandled message"),
        }
    }

    template.stop()
}

pub fn func_params_as_result(
    in_json: serde_json::Value,
) -> Result<Option<serde_json::Value>, Box<dyn Error + Send + Sync>> {
    let out_json = json!({
        "called function": "func_params_as_result",
        "your param was": in_json
    });

    Ok(Some(out_json))
}
