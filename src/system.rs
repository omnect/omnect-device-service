use anyhow::{bail, Context, Result};
use log::info;
use std::fs;
use std::path::Path;

static BOOTLOADER_UPDATED_FILE: &str = "/run/omnect-device-service/omnect_bootloader_updated";
static DEV_OMNECT: &str = "/dev/omnect/";

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
