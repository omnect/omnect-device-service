pub mod bootloader_env;
pub mod systemd;
mod test_util;
pub mod twin;
pub mod update_validation;

use azure_iot_sdk::client::*;
use env_logger::{Builder, Env, Target};
use log::{error, info};
use std::io::Write;
use std::process;
use twin::Twin;

#[tokio::main]
async fn main() {
    let mut builder;
    log_panics::init();

    if cfg!(debug_assertions) {
        builder = Builder::from_env(Env::default().default_filter_or("debug"));
    } else {
        builder = Builder::from_env(Env::default().default_filter_or("info"));
    }

    builder.format(|buf, record| {
        let level = match record.level() {
            log::Level::Info => 6,
            log::Level::Warn => 4,
            log::Level::Error => 3,
            _ => 7,
        };
        writeln!(buf, "<{level}>{}: {}", record.target(), record.args())
    });
    builder.target(Target::Stdout).init();

    info!(
        "module version: {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_SHORT_REV")
    );
    info!("azure sdk version: {}", IotHubClient::sdk_version_string());

    if let Err(e) = Twin::run(None).await {
        error!("application error: {e:#?}");

        process::exit(1);
    }

    info!("application shutdown")
}
