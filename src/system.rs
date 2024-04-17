use crate::{systemd, systemd::unit::UnitAction};

use anyhow::{Context, Result};
use log::info;
use std::fs;
use std::path::Path;
use std::time::Duration;

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";
static DEV_OMNECT: &str = "/dev/omnect/";
static NETWORK_SERVICE: &str = "systemd-networkd.service";
static NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS: u64 = 30;

fn info_current_root() -> Result<()> {
    let current_root = fs::read_link(DEV_OMNECT.to_owned() + "rootCurrent")
        .context("getting current root device")?;
    let root_a = fs::read_link(DEV_OMNECT.to_owned() + "rootA").context("getting rootA")?;
    let root_b = fs::read_link(DEV_OMNECT.to_owned() + "rootB").context("getting rootB")?;

    let root = if current_root == root_a {
        "a"
    } else if current_root == root_b {
        "b"
    } else {
        "unknown"
    };
    info!("device booted from root {root}.");
    Ok(())
}

fn info_bootloader_updated() {
    if let Ok(true) = Path::new(BOOTLOADER_UPDATED_FILE).try_exists() {
        info!("bootloader was updated.")
    }
}

pub fn infos() -> Result<()> {
    info_current_root()?;
    info_bootloader_updated();
    Ok(())
}

pub async fn restart_network() -> Result<()> {
    systemd::unit::unit_action(
        NETWORK_SERVICE,
        UnitAction::Reload,
        Duration::from_secs(NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS),
    )
    .await
}
