use crate::{systemd, systemd::unit::UnitAction};

use anyhow::{bail, Context, Result};
use log::info;
use serde_json::json;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";
static DEV_OMNECT: &str = "/dev/omnect/";
static NETWORK_SERVICE: &str = "systemd-networkd.service";
static NETWORK_SERVICE_RELOAD_TIMEOUT_IN_SECS: u64 = 15;

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

    let sw_versions = std::fs::read_to_string(path).context("versions: cannot read sw-versions")?;
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

pub fn boot_time() -> Result<String> {
    if cfg!(feature = "mock") {
        Ok("2024-10-10T05:27:52.804875461Z".to_string())
    } else {
        let mut s = String::new();
        std::fs::File::open("/proc/uptime")?.read_to_string(&mut s)?;
        let boot_time = s
            .trim()
            .split(' ')
            .take(1)
            .next()
            .context("boot_time: get uptime")?;
        let boot_time = OffsetDateTime::now_utc()
            - std::time::Duration::from_secs_f64(
                boot_time
                    .parse::<f64>()
                    .context("boot_time: parse uptime")?,
            );

        Ok(boot_time
            .format(&Rfc3339)
            .context("boot_time: format uptime")?)
    }
}
