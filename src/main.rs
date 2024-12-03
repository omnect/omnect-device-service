pub mod bootloader_env;
pub mod systemd;
pub mod twin;
pub mod update_validation;
pub mod web_service;

use azure_iot_sdk::client::*;
use env_logger::{Builder, Env, Target};
use log::{error, info};
use std::io::Write;
use std::process;
use twin::Twin;

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";
static DEV_OMNECT: &str = "/dev/omnect/";

#[tokio::main]
async fn main() {
    log_panics::init();

    let mut builder = if cfg!(debug_assertions) {
        Builder::from_env(Env::default().default_filter_or(
            "trace, \
            actix_server=error, \
            azure_iot_sdk=info, \
            eis_utils=info, \
            hyper=error, \
            hyper_util=error, \
            mio=error, \
            notify=error, \
            reqwest=error, \
            tracing=error",
        ))
    } else {
        Builder::from_env(Env::default().default_filter_or("info"))
    };

    builder.format(|buf, record| match record.level() {
        log::Level::Info => writeln!(buf, "<6>{}: {}", record.target(), record.args()),
        log::Level::Warn => writeln!(buf, "<4>{}: {}", record.target(), record.args()),
        log::Level::Error => {
            eprintln!("<3>{}: {}", record.target(), record.args());
            Ok(())
        }
        _ => writeln!(buf, "<7>{}: {}", record.target(), record.args()),
    });

    builder.target(Target::Stdout).init();

    info!(
        "module version: {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_SHORT_REV")
    );
    info!("azure sdk version: {}", IotHubClient::sdk_version_string());

    #[cfg(not(feature = "mock"))]
    if let Err(e) = Self::infos() {
        error!("application error: {e:#}");
    }

    if let Err(e) = Twin::run().await {
        error!("application error: {e:#}");

        process::exit(1);
    }

    info!("application shutdown")
}

#[allow(dead_code)]
fn infos() -> Result<()> {
    info!("bootloader was updated: {}", bootloader_updated());
    info!("device booted from root {}.", current_root()?);
    Ok(())
}

fn current_root() -> Result<&'static str> {
    let current_root = fs::read_link(DEV_OMNECT.to_owned() + "rootCurrent")
        .context("current_root: getting current root device")?;

    if current_root
        == fs::read_link(DEV_OMNECT.to_owned() + "rootA").context("current_root: getting rootA")?
    {
        return Ok("a");
    }

    if current_root
        == fs::read_link(DEV_OMNECT.to_owned() + "rootB").context("current_root: getting rootB")?
    {
        return Ok("b");
    }

    bail!("current_root: device booted from unknown root")
}

fn bootloader_updated() -> bool {
    Path::new(BOOTLOADER_UPDATED_FILE)
        .try_exists()
        .is_ok_and(|res| res)
}
