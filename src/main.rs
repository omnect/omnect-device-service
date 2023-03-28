use azure_iot_sdk::client::*;
use env_logger::{Builder, Env};
use log::{error, info};
use std::process;

fn main() {
    log_panics::init();

    if cfg!(debug_assertions) {
        Builder::from_env(Env::default().default_filter_or("debug")).init();
    } else {
        Builder::from_env(Env::default().default_filter_or("info")).init();
    }

    info!(
        "module version: {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_SHORT_REV")
    );
    info!(
        "azure sdk version: {}",
        IotHubClient::get_sdk_version_string()
    );

    if let Err(e) = omnect_device_service::run() {
        error!("Application error: {:#?}", e);

        process::exit(1);
    }
}
