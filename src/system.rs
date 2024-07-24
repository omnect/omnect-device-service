use crate::{systemd, systemd::unit::UnitAction};

use anyhow::{bail, Context, Result};
use log::info;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";
static DEV_OMNECT: &str = "/dev/omnect/";
static NETWORK_SERVICE: &str = "systemd-networkd.service";
static NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS: u64 = 30;

fn current_root() -> Result<&'static str> {
    let current_root = fs::read_link(DEV_OMNECT.to_owned() + "rootCurrent")
        .context("getting current root device")?;

    if current_root == fs::read_link(DEV_OMNECT.to_owned() + "rootA").context("getting rootA")? {
        return Ok("a");
    }

    if current_root == fs::read_link(DEV_OMNECT.to_owned() + "rootB").context("getting rootB")? {
        return Ok("b");
    }

    bail!("device booted from unknown root")
}

fn bootloader_updated() -> bool {
    Path::new(BOOTLOADER_UPDATED_FILE)
        .try_exists()
        .is_ok_and(|res| res)
}

pub fn infos() -> Result<()> {
    info!("bootloader was updated: {}", bootloader_updated());
    info!("device booted from root {}.", current_root()?);
    Ok(())
}

pub async fn reload_network() -> Result<()> {
    systemd::unit::unit_action(
        NETWORK_SERVICE,
        UnitAction::Reload,
        Duration::from_secs(NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS),
    )
    .await
}

pub fn sw_version() -> Result<serde_json::Value> {
    let path = if cfg!(feature = "mock") {
        "testfiles/positive/sw-versions"
    } else {
        "/etc/sw-versions"
    };

    let sw_versions = std::fs::read_to_string(path).context("cannot read sw-versions")?;
    let sw_versions: Vec<&str> = sw_versions.trim_end().split(' ').collect();

    anyhow::ensure!(
        sw_versions.len() == 2,
        "sw-versions: unexpected number of entries"
    );

    Ok(json!( {
        "osName": sw_versions[0],
        "swVersion": sw_versions[1],
    }))
}

pub fn provisioning_method() -> Result<serde_json::Value> {
    let path = "testfiles/positive/config.toml.est";
    let config: toml::map::Map<String, toml::Value> = std::fs::read_to_string(path).context(format!("cannot read {}", path))?.parse::<toml::Table>()?;

    info!("{:?}", config);
    let source = &config["provisioning"]["source"];
    let method = &config["provisioning"]["attestation"]["method"];


    Ok(json!( {
        "bla": "blub"
    }))
}