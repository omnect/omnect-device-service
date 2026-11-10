mod adu_types;
mod common;
mod os_version;
pub mod update_validation;

use crate::{
    bootloader_env,
    common::{RootPartition, from_json_file, path_ends_with, to_json_file},
    systemd,
    systemd::{unit::UnitAction, watchdog::WatchdogManager},
    twin::{
        Feature,
        feature::*,
        firmware_update::{adu_types::*, common::*, os_version::*},
    },
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use log::{debug, error, info, warn};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tar::Archive;
use update_validation::UpdateValidation;

static LOAD_UPDATE_WDT_INTERVAL_SECS: u64 = 120;
static RUN_UPDATE_WDT_INTERVAL_SECS: u64 = 600;
static BOOTLOADER_UPDATED_FLAG: &str = "1";
static ADU_USER: &str = "adu";
#[cfg(not(feature = "mock"))]
static RM_BIN: &str = "/bin/rm";

struct LoadUpdateGuard {
    wdt: Option<Duration>,
}

impl LoadUpdateGuard {
    async fn new() -> Result<Self> {
        let wdt =
            WatchdogManager::interval(Duration::from_secs(LOAD_UPDATE_WDT_INTERVAL_SECS)).await?;

        debug!("changed wdt to {LOAD_UPDATE_WDT_INTERVAL_SECS}s and saved old one ({wdt:?})");

        Ok(LoadUpdateGuard { wdt })
    }
}

impl Drop for LoadUpdateGuard {
    fn drop(&mut self) {
        let wdt = self.wdt.take();

        debug!("load update: restore old wdt ({wdt:?})");

        tokio::spawn(async move {
            if let Some(wdt) = wdt
                && let Err(e) = WatchdogManager::interval(wdt).await
            {
                error!("failed to restore wdt interval: {e:#}")
            }
        });
    }
}

fn merge_bootargs(omnect: &str, custom: &str) -> String {
    omnect
        .split_whitespace()
        .chain(custom.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

struct RunUpdateGuard {
    succeeded: bool,
    wdt: Option<Duration>,
    target_partition: RootPartition,
    bootloader_updated: bool,
    bootargs_omnect_backup: Option<PathBuf>,
}

impl RunUpdateGuard {
    async fn new(target_partition: RootPartition) -> Result<Self> {
        let succeeded = false;
        let wdt =
            WatchdogManager::interval(Duration::from_secs(RUN_UPDATE_WDT_INTERVAL_SECS)).await?;

        debug!("changed wdt to {RUN_UPDATE_WDT_INTERVAL_SECS}s and saved old one ({wdt:?})");

        systemd::unit::unit_action(
            IOT_HUB_DEVICE_UPDATE_SERVICE_TIMER,
            UnitAction::Stop,
            systemd_zbus::Mode::Replace,
        )
        .await?;
        systemd::unit::unit_action(
            IOT_HUB_DEVICE_UPDATE_SERVICE,
            UnitAction::Stop,
            systemd_zbus::Mode::Replace,
        )
        .await?;

        debug!("stopped {IOT_HUB_DEVICE_UPDATE_SERVICE}");

        Ok(RunUpdateGuard {
            succeeded,
            wdt,
            target_partition,
            bootloader_updated: false,
            bootargs_omnect_backup: None,
        })
    }

    fn finalize(&mut self) {
        self.succeeded = true;
    }

    /// Rollback only logs errors and doesn't return them. Returns whether the
    /// update partition may still be selected. Extracted as a free-standing
    /// associated function so `Drop` can move the fields into `spawn_blocking`
    /// without borrowing `self`.
    fn do_rollback(
        bootloader_updated: bool,
        target_partition: RootPartition,
        bootargs_omnect_backup: Option<PathBuf>,
    ) -> bool {
        if bootloader_updated {
            // the flashed bootloader cannot be rolled back and must not boot the current rootfs
            // again, and rootfs and kernelargs are already installed: complete the switch
            warn!("bootloader was updated: rollback not possible, switching to the new rootfs");
            if let Err(e) = FirmwareUpdate::switch_to_update_part(target_partition) {
                error!("failed to switch to the new rootfs: {e:#}");
            }
            true
        } else if let Err(e) = bootloader_env::unset(OMNECT_VALIDATE_UPDATE_PART) {
            // the bootargs and the config must stay as long as the update partition may still be
            // selected for a validated boot
            error!("failed to unset {OMNECT_VALIDATE_UPDATE_PART}: {e:#}");
            true
        } else {
            let _ = fs::remove_file(update_validation_config_path!());
            Self::restore_bootargs(bootargs_omnect_backup);
            false
        }
    }

    fn restore_bootargs(bootargs_omnect_backup: Option<PathBuf>) {
        // the backup is kept: it holds the args of the current rootfs
        if let Err(e) = bootloader_env::unset(OMNECT_VALIDATE_EXTRA_BOOTARGS) {
            error!("failed to unset {OMNECT_VALIDATE_EXTRA_BOOTARGS}: {e:#}");
        }

        if let Some(backup) = bootargs_omnect_backup
            && let Err(e) = Self::restore_omnect_file(&backup)
        {
            error!("failed to restore omnect bootargs file from backup: {e:#}");
        }
    }

    fn restore_omnect_file(backup: &Path) -> std::io::Result<()> {
        let omnect_file = bootargs_omnect_file_path!();
        let saved = fs::read(backup)?;

        // leave /boot untouched if the run failed before changing it; a missing file was backed
        // up as an empty one
        match fs::read(&omnect_file) {
            Ok(current) if current == saved => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && saved.is_empty() => {
                return Ok(());
            }
            _ => {}
        }

        fs::write(omnect_file, saved)
    }
}

impl Drop for RunUpdateGuard {
    fn drop(&mut self) {
        if !(self.succeeded) {
            let wdt = self.wdt.take();
            let target_partition = self.target_partition;
            let bootloader_updated = self.bootloader_updated;
            let bootargs_backup = self.bootargs_omnect_backup.take();

            debug!("run update failed: roll back and restore old wdt ({wdt:?})");

            tokio::spawn(async move {
                let update_part_selected = tokio::task::spawn_blocking(move || {
                    Self::do_rollback(bootloader_updated, target_partition, bootargs_backup)
                })
                .await
                .unwrap_or_else(|e| {
                    error!("rollback task panicked: {e:#}");
                    true
                });

                if let Some(wdt) = wdt
                    && let Err(e) = WatchdogManager::interval(wdt).await
                {
                    error!("failed to restore wdt interval: {e:#}")
                }

                // an ADU update would write the partition the next boot may select
                if update_part_selected {
                    warn!(
                        "update partition may be selected: {IOT_HUB_DEVICE_UPDATE_SERVICE} stays stopped until reboot"
                    );
                    return;
                }

                debug!(
                    "restart {IOT_HUB_DEVICE_UPDATE_SERVICE} and {IOT_HUB_DEVICE_UPDATE_SERVICE_TIMER}"
                );

                if let Err(e) = systemd::unit::unit_action(
                    IOT_HUB_DEVICE_UPDATE_SERVICE,
                    UnitAction::Start,
                    systemd_zbus::Mode::Fail,
                )
                .await
                {
                    error!("failed to restart {IOT_HUB_DEVICE_UPDATE_SERVICE}: {e:#}")
                }

                if let Err(e) = systemd::unit::unit_action(
                    IOT_HUB_DEVICE_UPDATE_SERVICE_TIMER,
                    UnitAction::Start,
                    systemd_zbus::Mode::Fail,
                )
                .await
                {
                    error!("failed to restart {IOT_HUB_DEVICE_UPDATE_SERVICE_TIMER}: {e:#}")
                }
            });
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct LoadUpdateCommand {
    pub update_file_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RunUpdateCommand {
    pub validate_iothub_connection: bool,
}

pub struct FirmwareUpdate {
    swu_file_path: Option<PathBuf>,
    update_validation: UpdateValidation,
}

impl Drop for FirmwareUpdate {
    fn drop(&mut self) {
        if let Err(e) = Self::clean_working_dir() {
            error!("failed to clean working directory: {e:#}")
        }
    }
}

impl Feature for FirmwareUpdate {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::FIRMWARE_UPDATE_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_FIRMWARE_UPDATE") != Ok("true".to_string())
    }

    async fn connect_web_service(&self) -> Result<()> {
        self.update_validation.report().await;
        Ok(())
    }

    async fn command(&mut self, cmd: &Command) -> CommandResult {
        match cmd {
            Command::LoadFirmwareUpdate(cmd) => self.load(&cmd.update_file_path).await,
            Command::RunFirmwareUpdate(cmd) => self.run(cmd.validate_iothub_connection).await,
            Command::ValidateUpdate(authenticated) => {
                self.update_validation
                    .set_authenticated(*authenticated)
                    .await?;
                Ok(None)
            }
            _ => bail!("unexpected command"),
        }
    }
}

impl FirmwareUpdate {
    const FIRMWARE_UPDATE_VERSION: u8 = 1;
    const ID: &'static str = "firmware_update";

    pub async fn new() -> Result<Self> {
        Ok(FirmwareUpdate {
            swu_file_path: None,
            update_validation: UpdateValidation::new().await?,
        })
    }

    async fn load<P>(&mut self, path: P) -> CommandResult
    where
        P: AsRef<Path>,
    {
        self.swu_file_path = None;

        let _guard = LoadUpdateGuard::new().await?;
        let du_config: DeviceUpdateConfig = from_json_file(du_config_path!())?;
        let current_version = OmnectOsVersion::from_sw_versions_file()?;
        let mut ar = Archive::new(fs::File::open(path).context("failed to open archive")?);
        let mut swu_path = None;
        let mut swu_sha = String::from("");
        let mut manifest_path = None;
        let mut manifest_sha1 = String::from("");
        let mut manifest_sha2 = String::from("");

        Self::clean_working_dir().context("failed to clean working directory")?;

        for file in ar.entries().context("failed to get archive entries")? {
            let mut file = file.context("failed to get archive entry")?;
            let path = file.path().context("failed to get entry path")?;

            debug!("extract entry: {path:?}");

            ensure!(
                path.parent().is_some_and(|p| p == Path::new("")),
                "entry path expected at root of archive"
            );

            let path = Path::new(&update_folder_path!()).join(path);

            if path_ends_with(&path, ".swu") {
                file.unpack(&path).context("failed to unpack *.swu")?;
                swu_sha = BASE64_STANDARD.encode(Sha256::digest(
                    std::fs::read(&path).context("failed to read *.swu for hash")?,
                ));
                swu_path = Some(path);
            } else if path_ends_with(&path, ".swu.importManifest.json") {
                file.unpack(&path)
                    .context("failed to unpack *.swu.importManifest.json")?;
                manifest_sha1 = Sha256::digest(
                    std::fs::read(&path)
                        .context("failed to read *.swu.importManifest.json for hash")?,
                )
                .iter()
                .fold(String::new(), |mut s, b| {
                    use std::fmt::Write;
                    let _ = write!(s, "{b:02X}");
                    s
                });
                manifest_path = Some(path);
            } else if path_ends_with(&path, ".swu.importManifest.json.sha256") {
                file.unpack(&path)
                    .context("failed to unpack *.swu.importManifest.json.sha256")?;
                manifest_sha2 = fs::read_to_string(path)
                    .context("failed to read *.swu.importManifest.json.sha256")?
                    .split_whitespace()
                    .next()
                    .context("failed to read first string of *.swu.importManifest.json.sha256")?
                    .to_owned();
            } else {
                error!("found unexpected entry");
            }
        }

        // ensure manifest hash matches
        ensure!(
            manifest_sha1.eq_ignore_ascii_case(manifest_sha2.trim()),
            "failed to verify *.swu.importManifest.json hash"
        );

        let Some(manifest_path) = manifest_path else {
            bail!("*.swu.importManifest.json missing");
        };

        let Some(swu_path) = swu_path else {
            bail!("*.swu missing");
        };

        let Some(swu_filename) = swu_path.file_name() else {
            bail!("failed to get *.swu filename from path");
        };

        // read manifest
        let manifest: ImportManifest = from_json_file(&manifest_path)?;

        // ensure swu hash
        let Some(file) = manifest
            .files
            .iter()
            .find(|f| swu_filename.eq(f.filename.as_str()))
        else {
            bail!("failed to find *.swu in manifest")
        };

        ensure!(
            file.hashes["sha256"].eq_ignore_ascii_case(swu_sha.trim()),
            "failed to verify *.swu hash"
        );

        ensure!(
            du_config.agents[0].manufacturer == manifest.compatibility[0].manufacturer,
            "failed to verify compatibility: manufacturer"
        );
        ensure!(
            du_config.agents[0].model == manifest.compatibility[0].model,
            "failed to verify compatibility: model"
        );
        ensure!(
            du_config.agents[0]
                .additional_device_properties
                .compatibilityid
                == manifest.compatibility[0].compatibilityid,
            "failed to verify compatibility: compatibilityid"
        );

        let new_version = OmnectOsVersion::from_string(&manifest.update_id.version)?;

        if current_version == new_version {
            bail!("version {current_version} already installed")
        }

        if current_version > new_version {
            bail!("downgrades not allowed ({new_version} < {current_version} )")
        }

        info!(
            "successfully loaded update (current version: {current_version} new version: {new_version})"
        );

        self.swu_file_path = Some(swu_path);

        Ok(Some(
            serde_json::to_value(manifest).context("failed to serialize manifest")?,
        ))
    }

    async fn run(&mut self, validate_iothub_connection: bool) -> CommandResult {
        #[cfg_attr(feature = "mock", allow(unused_variables))]
        let Some(ref swu_file_path) = self.swu_file_path else {
            bail!("no update loaded")
        };

        let target_partition = RootPartition::current()?.other();

        Self::ensure_no_pending_switch(target_partition)?;

        let mut guard = RunUpdateGuard::new(target_partition).await?;

        guard.bootargs_omnect_backup =
            Some(Self::backup_bootargs_once().context("failed to back up omnect bootargs file")?);

        #[cfg(not(feature = "mock"))]
        Self::swupdate(swu_file_path, target_partition.root_update_params()).context(format!(
            "failed to update root partition: swupdate logs at {}",
            log_file_path!()
        ))?;

        #[cfg(not(feature = "mock"))]
        Self::swupdate(swu_file_path, target_partition.kernelargs_update_params()).context(
            format!(
                "failed to update kernelargs: swupdate logs at {}",
                log_file_path!()
            ),
        )?;

        #[cfg(not(feature = "mock"))]
        Self::bootloader_step(&mut guard, || {
            Self::swupdate(swu_file_path, target_partition.bootloader_update_params())
        })?;

        Self::activate_update(&mut guard, validate_iothub_connection)?;

        systemd::reboot("swupdate", "local update").await?;

        info!("update succeeded");

        Ok(None)
    }

    fn activate_update(guard: &mut RunUpdateGuard, validate_iothub_connection: bool) -> Result<()> {
        let target_partition = guard.target_partition;

        if guard.bootloader_updated {
            Self::switch_to_update_part(target_partition)?;
        } else {
            Self::apply_bootargs(false)?;
            Self::set_boot_partition(target_partition, validate_iothub_connection)?;
        }

        // finalize right after the boot partition is switched: once the next boot
        // selects the new partition no rollback may run, even if the reboot call fails
        guard.finalize();

        Ok(())
    }

    fn switch_to_update_part(target_partition: RootPartition) -> Result<()> {
        bootloader_env::set(OMNECT_OS_BOOTPART, &target_partition.index().to_string())?;
        // the selected rootfs needs its args even if the flag fails
        let flagged = bootloader_env::set(OMNECT_BOOTLOADER_UPDATED, BOOTLOADER_UPDATED_FLAG);
        Self::apply_bootargs(true)?;
        flagged
    }

    fn marker_exists(marker: &str) -> std::io::Result<bool> {
        Path::new(marker).try_exists()
    }

    fn ensure_no_pending_switch(target_partition: RootPartition) -> Result<()> {
        // writing the target partition again after it was selected or after a bootloader update
        // could leave no complete rootfs to boot, and stale env of the first activation could be
        // picked up
        let target_index = target_partition.index().to_string();
        // a failed read counts as not selected
        let selects_target =
            |key: &str| bootloader_env::get(key).is_ok_and(|part| part == target_index);

        let updated_marker = bootloader_updated_file_path!();
        let bootloader_updated = Self::marker_exists(&updated_marker).with_context(|| {
            format!(
                "failed to check {updated_marker}: cannot tell whether a bootloader update ran in this boot"
            )
        })?;

        if bootloader_updated {
            // the marker is created in the bootloader step, after rootfs and kernelargs were
            // installed: finish a switch the rollback may have failed to do
            if let Err(e) = Self::switch_to_update_part(target_partition) {
                error!("failed to switch to the new rootfs: {e:#}");
            }
            ensure!(
                selects_target(OMNECT_OS_BOOTPART),
                "bootloader update in this boot, but the new rootfs is not selected: do not reboot, the current rootfs may not start with the new bootloader"
            );
            bail!("bootloader update in this boot: reboot before installing another update");
        }

        ensure!(
            !selects_target(OMNECT_VALIDATE_UPDATE_PART),
            "update activated in this boot: reboot before installing another update"
        );

        ensure!(
            !selects_target(OMNECT_OS_BOOTPART),
            "update pending in this boot: install another update only after the update validation has completed"
        );

        Ok(())
    }

    fn backup_bootargs_once() -> Result<PathBuf> {
        // an existing backup is from an earlier attempt in this boot and holds the args of the
        // current rootfs
        let backup_file = PathBuf::from(bootargs_omnect_backup_file_path!());

        if !backup_file.try_exists()? {
            let omnect_file = bootargs_omnect_file_path!();
            // a missing omnect file has the same effect as an empty one, and /boot stays untouched
            // until the first swupdate
            let result = if Path::new(&omnect_file).try_exists()? {
                fs::copy(&omnect_file, &backup_file).map(|_| ())
            } else {
                fs::write(&backup_file, "")
            };
            if let Err(e) = result {
                let _ = fs::remove_file(&backup_file);
                return Err(e.into());
            }
        }

        Ok(backup_file)
    }

    #[cfg_attr(all(feature = "mock", not(test)), allow(dead_code))]
    fn bootloader_step(
        guard: &mut RunUpdateGuard,
        swupdate: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        // a stale marker would hide a failed bootloader step
        Self::remove_marker(&no_bootloader_updated_file_path!())
            .context("failed to remove the not-necessary marker of an earlier attempt")?;

        let swupdate_succeeded = swupdate()
            .inspect_err(|e| {
                info!(
                    "bootloader swupdate returned an error: {e:#} (swupdate logs at {})",
                    log_file_path!()
                )
            })
            .is_ok();

        let updated_marker = bootloader_updated_file_path!();
        let bootloader_marker = Self::marker_exists(&updated_marker);
        let marker_kept = !swupdate_succeeded
            && !matches!(bootloader_marker, Ok(false))
            && Self::remove_marker(&updated_marker)
                .inspect_err(|e| error!("failed to remove the bootloader marker: {e:#}"))
                .is_err();
        guard.bootloader_updated = swupdate_succeeded || marker_kept;
        Self::ensure_bootloader_markers(swupdate_succeeded, bootloader_marker)
    }

    #[cfg_attr(all(feature = "mock", not(test)), allow(dead_code))]
    fn remove_marker(marker: &str) -> Result<()> {
        // the hook creates the markers as adu in the sticky /tmp
        #[cfg(not(feature = "mock"))]
        ensure!(
            Self::adu_command(RM_BIN)
                .arg("-f")
                .arg(marker)
                .status()?
                .success(),
            "failed to run rm command"
        );

        #[cfg(feature = "mock")]
        if let Err(e) = fs::remove_file(marker)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.into());
        }

        Ok(())
    }

    #[cfg_attr(all(feature = "mock", not(test)), allow(dead_code))]
    fn ensure_bootloader_markers(
        swupdate_succeeded: bool,
        bootloader_marker: std::io::Result<bool>,
    ) -> Result<()> {
        let updated_marker = bootloader_updated_file_path!();

        if swupdate_succeeded {
            if !matches!(bootloader_marker, Ok(true)) {
                warn!(
                    "bootloader updated, but {updated_marker} is missing or cannot be checked: switching to the new rootfs without validation"
                );
            }
            return Ok(());
        }

        let bootloader_marker = bootloader_marker.with_context(|| {
            format!(
                "failed to update bootloader: cannot tell whether the bootloader was written (swupdate logs at {})",
                log_file_path!()
            )
        })?;

        // the sw-description hook creates the marker before the bootloader is written, so /boot
        // may be partly written
        ensure!(
            !bootloader_marker,
            "failed to update bootloader: writing failed after {updated_marker} was created. (swupdate logs at {})",
            log_file_path!()
        );

        let not_necessary_marker = no_bootloader_updated_file_path!();
        ensure!(
            Self::marker_exists(&not_necessary_marker).with_context(|| {
                format!(
                    "failed to update bootloader: cannot tell whether a bootloader update was necessary (swupdate logs at {})",
                    log_file_path!()
                )
            })?,
            "failed to update bootloader: expected {not_necessary_marker} to be present. (swupdate logs at {})",
            log_file_path!()
        );

        Ok(())
    }

    fn set_boot_partition(
        target_partition: RootPartition,
        validate_iothub_connection: bool,
    ) -> Result<()> {
        to_json_file(
            &UpdateValidationConfig {
                local: !validate_iothub_connection,
            },
            update_validation_config_path!(),
            true,
        )?;
        bootloader_env::set(
            OMNECT_VALIDATE_UPDATE_PART,
            &target_partition.index().to_string(),
        )
    }

    fn apply_bootargs(bootloader_updated: bool) -> Result<()> {
        let current_bootargs = bootloader_env::get(OMNECT_EXTRA_BOOTARGS).unwrap_or_default();
        let omnect_bootargs = fs::read_to_string(bootargs_omnect_file_path!())?; // has to exist
        let custom_bootargs = match fs::read_to_string(bootargs_custom_file_path!()) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(), // optional: missing file
            Err(e) => return Err(e.into()),
        };
        let new_bootargs = merge_bootargs(&omnect_bootargs, &custom_bootargs);

        if current_bootargs != new_bootargs {
            match (bootloader_updated, new_bootargs.is_empty()) {
                (true, true) => bootloader_env::unset(OMNECT_EXTRA_BOOTARGS)?,
                (true, false) => bootloader_env::set(OMNECT_EXTRA_BOOTARGS, &new_bootargs)?,
                (false, true) => {
                    bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, NOARGS_SENTINEL)?
                }
                (false, false) => {
                    bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, &new_bootargs)?
                }
            }
        } else if !bootloader_updated {
            // a value staged by an earlier attempt, e.g. left by a failed rollback, would
            // otherwise be applied on the trial boot
            bootloader_env::unset(OMNECT_VALIDATE_EXTRA_BOOTARGS)?;
        }
        Ok(())
    }

    #[cfg_attr(feature = "mock", allow(dead_code))]
    fn swupdate<P>(swu_file_path: P, selection: &str) -> Result<()>
    where
        P: AsRef<std::ffi::OsStr>,
    {
        let stdio_logfile = std::fs::OpenOptions::new()
            .write(true)
            .open(log_file_path!())
            .context("failed to open for write log file")?;

        let stderr_logfile = stdio_logfile
            .try_clone()
            .context("failed to stdio clone log file handle")?;

        ensure!(
            Self::adu_command("swupdate")
                .arg("-v")
                .arg("-i")
                .arg(&swu_file_path)
                .arg("-k")
                .arg(pubkey_file_path!())
                .arg("-e")
                .arg(selection)
                .current_dir("/usr/bin")
                .stdout(std::process::Stdio::from(stdio_logfile))
                .stderr(std::process::Stdio::from(stderr_logfile))
                .status()?
                .success(),
            "failed to run swupdate command"
        );
        Ok(())
    }

    fn adu_command(bin: &str) -> std::process::Command {
        let mut command = std::process::Command::new(bootloader_env::SUDO_BIN);
        command.arg("-u").arg(ADU_USER).arg(bin);
        command
    }

    fn clean_working_dir() -> Result<()> {
        for entry in fs::read_dir(update_folder_path!())? {
            let entry = entry?;
            let path = entry.path();

            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;

    use crate::bootloader_env::TEST_LOCK as BOOTARGS_TEST_LOCK;

    // helper
    fn setup_bootargs_files(tmp: &tempfile::TempDir, omnect: &str, custom: &str) {
        let omnect_path = tmp.path().join("bootargs_omnect");
        let custom_path = tmp.path().join("bootargs_custom");
        fs::write(&omnect_path, omnect).unwrap();
        fs::write(&custom_path, custom).unwrap();
        crate::common::set_env_var("BOOTARGS_OMNECT_FILE_PATH", &omnect_path);
        crate::common::set_env_var("BOOTARGS_CUSTOM_FILE_PATH", &custom_path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_ok() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let update_folder = tmp_dir.path().join("local_update");
        let du_config_file = tmp_dir.path().join("du-config.json");
        let sw_versions_file = tmp_dir.path().join("sw-versions");
        std::fs::copy("testfiles/positive/du-config.json", &du_config_file).unwrap();
        std::fs::copy("testfiles/positive/sw-versions", &sw_versions_file).unwrap();
        std::fs::create_dir_all(&update_folder).unwrap();
        crate::common::set_env_var("UPDATE_FOLDER_PATH", update_folder);
        crate::common::set_env_var("DEVICE_UPDATE_PATH", du_config_file);
        crate::common::set_env_var("SW_VERSIONS_PATH", sw_versions_file);
        let update_validation = UpdateValidation::new().await.unwrap();

        let mut firmware_update = FirmwareUpdate {
            swu_file_path: None,
            update_validation,
        };

        firmware_update
            .load("testfiles/positive/update.tar")
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_sw_versions_fail() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let update_folder = tmp_dir.path().join("local_update");
        let du_config_file = tmp_dir.path().join("du-config.json");
        let sw_versions_file = tmp_dir.path().join("sw-versions");
        std::fs::copy("testfiles/positive/du-config.json", &du_config_file).unwrap();
        std::fs::create_dir_all(&update_folder).unwrap();
        crate::common::set_env_var("UPDATE_FOLDER_PATH", update_folder);
        crate::common::set_env_var("DEVICE_UPDATE_PATH", du_config_file);
        crate::common::set_env_var("SW_VERSIONS_PATH", &sw_versions_file);
        let update_validation = UpdateValidation::new().await.unwrap();

        let mut firmware_update = FirmwareUpdate {
            swu_file_path: None,
            update_validation,
        };

        fs::write(&sw_versions_file, "dobi-OMNECT-gateway-devel 40.0.0.0").unwrap();

        let err = firmware_update
            .load("testfiles/positive/update.tar")
            .await
            .unwrap_err();

        assert!(
            err.chain()
                .any(|e| e.to_string().starts_with("downgrades not allowed"))
        );

        fs::write(
            sw_versions_file,
            "dobi-OMNECT-gateway-devel 4.0.24.557123921",
        )
        .unwrap();

        let err = firmware_update
            .load("testfiles/positive/update.tar")
            .await
            .unwrap_err();

        assert!(err.chain().any(|e| {
            e.to_string()
                .starts_with("version 4.0.24.557123921 already installed")
        }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_compatibility_fail() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let update_folder = tmp_dir.path().join("local_update");
        let du_config_file = tmp_dir.path().join("du-config.json");
        let sw_versions_file = tmp_dir.path().join("sw-versions");
        std::fs::copy("testfiles/positive/sw-versions", &sw_versions_file).unwrap();
        std::fs::create_dir_all(&update_folder).unwrap();
        crate::common::set_env_var("UPDATE_FOLDER_PATH", update_folder);
        crate::common::set_env_var("DEVICE_UPDATE_PATH", &du_config_file);
        crate::common::set_env_var("SW_VERSIONS_PATH", sw_versions_file);
        let update_validation = UpdateValidation::new().await.unwrap();

        let mut firmware_update = FirmwareUpdate {
            swu_file_path: None,
            update_validation,
        };

        let mut du_config = DeviceUpdateConfig {
            agents: vec![Agent {
                manufacturer: "".to_string(),
                model: "".to_string(),
                additional_device_properties: AdditionalDeviceProperties {
                    compatibilityid: "".to_string(),
                },
            }],
        };

        fs::write(
            &du_config_file,
            serde_json::to_string_pretty(&du_config).unwrap(),
        )
        .unwrap();

        let err = firmware_update
            .load("testfiles/positive/update.tar")
            .await
            .unwrap_err();

        assert!(err.chain().any(|e| {
            e.to_string()
                .starts_with("failed to verify compatibility: manufacturer")
        }));

        du_config.agents[0].manufacturer = "conplement-ag".to_string();

        fs::write(
            &du_config_file,
            serde_json::to_string_pretty(&du_config).unwrap(),
        )
        .unwrap();

        let err = firmware_update
            .load("testfiles/positive/update.tar")
            .await
            .unwrap_err();

        assert!(err.chain().any(|e| {
            e.to_string()
                .starts_with("failed to verify compatibility: model")
        }));

        du_config.agents[0].model = "omnect-raspberrypi4-64-gateway-devel".to_string();

        fs::write(
            &du_config_file,
            serde_json::to_string_pretty(&du_config).unwrap(),
        )
        .unwrap();

        let err = firmware_update
            .load("testfiles/positive/update.tar")
            .await
            .unwrap_err();

        assert!(err.chain().any(|e| {
            e.to_string()
                .starts_with("failed to verify compatibility: compatibilityid")
        }));
    }

    #[test]
    fn bootargs_no_update_new_args_sets_validate_key() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();
        setup_bootargs_files(&tmp, "console=ttyS0,115200", "loglevel=7");

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").unwrap();
        FirmwareUpdate::apply_bootargs(false).unwrap();

        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS).unwrap(),
            "console=ttyS0,115200 loglevel=7"
        );
        // original key must be untouched
        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).unwrap(),
            "old_value"
        );
    }

    #[test]
    fn bootargs_no_update_empty_args_sets_noargs_sentinel() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();
        setup_bootargs_files(&tmp, "   ", ""); // whitespace only → normalizes to empty

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").unwrap();
        FirmwareUpdate::apply_bootargs(false).unwrap();

        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS).unwrap(),
            NOARGS_SENTINEL
        );
    }

    #[test]
    fn bootargs_bootloader_updated_new_args_sets_extra_bootargs() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();
        setup_bootargs_files(&tmp, "quiet", "systemd.log_level=debug");

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").unwrap();
        FirmwareUpdate::apply_bootargs(true).unwrap();

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).unwrap(),
            "quiet systemd.log_level=debug"
        );
    }

    #[test]
    fn bootargs_bootloader_updated_empty_args_unsets_extra_bootargs() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();
        setup_bootargs_files(&tmp, "", "");

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").unwrap();
        FirmwareUpdate::apply_bootargs(true).unwrap();

        assert!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS)
                .unwrap()
                .is_empty(),
            "expected omnect_extra_bootargs to be unset"
        );
    }

    #[test]
    fn bootargs_whitespace_is_normalized() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();
        setup_bootargs_files(&tmp, "  arg1   arg2\n", "\n  arg3  ");

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").unwrap();
        FirmwareUpdate::apply_bootargs(true).unwrap();

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).unwrap(),
            "arg1 arg2 arg3"
        );
    }

    #[test]
    fn bootargs_unchanged_does_not_write_env() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();
        setup_bootargs_files(&tmp, "stable_arg", "");

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "stable_arg").unwrap();
        FirmwareUpdate::apply_bootargs(false).unwrap();

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).unwrap(),
            "stable_arg"
        );
        assert!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .unwrap()
                .is_empty(),
            "omnect_validate_extra_bootargs was unexpectedly set"
        );
    }

    #[test]
    fn bootargs_unchanged_unsets_stale_validate_key() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        setup_bootargs_files(&tmp, "stable_arg", "");

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "stable_arg").expect("set extra bootargs");
        bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, "stale")
            .expect("set validate extra bootargs");
        FirmwareUpdate::apply_bootargs(false).expect("apply bootargs");

        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate extra bootargs"),
            "",
            "expected the stale staged bootargs to be unset"
        );
    }

    #[test]
    fn bootargs_missing_omnect_file_treated_as_error() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();

        // omnect file is missing → apply_bootargs must return Err
        crate::common::set_env_var("BOOTARGS_OMNECT_FILE_PATH", tmp.path().join("no_omnect"));
        let custom_path = tmp.path().join("bootargs_custom");
        fs::write(&custom_path, "loglevel=7").unwrap();
        crate::common::set_env_var("BOOTARGS_CUSTOM_FILE_PATH", &custom_path);

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").unwrap();

        assert!(FirmwareUpdate::apply_bootargs(false).is_err());
    }

    #[test]
    fn bootargs_missing_custom_file_treated_as_empty() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();

        // omnect file exists with a value, custom file is missing → treated as ""
        let omnect_path = tmp.path().join("bootargs_omnect");
        fs::write(&omnect_path, "console=ttyS0,115200").unwrap();
        crate::common::set_env_var("BOOTARGS_OMNECT_FILE_PATH", &omnect_path);
        crate::common::set_env_var("BOOTARGS_CUSTOM_FILE_PATH", tmp.path().join("no_custom"));

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").unwrap();
        FirmwareUpdate::apply_bootargs(false).unwrap();

        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS).unwrap(),
            "console=ttyS0,115200"
        );
    }

    fn make_guard(
        succeeded: bool,
        bootloader_updated: bool,
        bootargs_omnect_backup: Option<PathBuf>,
    ) -> RunUpdateGuard {
        RunUpdateGuard {
            succeeded,
            wdt: None,
            target_partition: RootPartition::B,
            bootloader_updated,
            bootargs_omnect_backup,
        }
    }

    fn rollback(guard: &mut RunUpdateGuard) -> bool {
        RunUpdateGuard::do_rollback(
            guard.bootloader_updated,
            guard.target_partition,
            guard.bootargs_omnect_backup.take(),
        )
    }

    fn setup_rollback_files(
        tmp: &tempfile::TempDir,
        omnect: &str,
        custom: &str,
    ) -> (PathBuf, PathBuf) {
        let omnect_path = tmp.path().join("bootargs_omnect");
        let custom_path = tmp.path().join("bootargs_custom");
        let backup_path = tmp.path().join("bootargs_omnect.backup");
        fs::write(&omnect_path, omnect).unwrap();
        fs::write(&custom_path, custom).unwrap();
        fs::write(&backup_path, omnect).unwrap();
        crate::common::set_env_var("BOOTARGS_OMNECT_FILE_PATH", &omnect_path);
        crate::common::set_env_var("BOOTARGS_CUSTOM_FILE_PATH", &custom_path);
        crate::common::set_env_var("BOOTARGS_OMNECT_BACKUP_FILE_PATH", &backup_path);
        // do_rollback removes the validation config: keep it away from the host's path
        crate::common::set_env_var(
            "UPDATE_VALIDATION_CONFIG_PATH",
            tmp.path().join("update_validation_conf.json"),
        );
        (omnect_path, backup_path)
    }

    fn setup_validation_env() {
        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").expect("set extra bootargs");
        bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, "staged")
            .expect("set validate extra bootargs");
        bootloader_env::set(
            OMNECT_VALIDATE_UPDATE_PART,
            &RootPartition::B.index().to_string(),
        )
        .expect("set validate update part");
    }

    #[test]
    fn rollback_restores_bootargs_file_from_backup() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_rollback_files(&tmp, "original_arg", "custom_arg");

        // simulate kernelargs swupdate overwriting the omnect file
        fs::write(&omnect_path, "corrupted").expect("write omnect file");
        setup_validation_env();

        assert!(
            !rollback(&mut make_guard(true, false, Some(backup_path.clone()))),
            "expected no selected update partition after the revert"
        );

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra bootargs"),
            "old_value",
            "expected omnect_extra_bootargs to be untouched"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate extra bootargs"),
            "",
            "expected validate key to be unset"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_UPDATE_PART).expect("get validate update part"),
            "",
            "expected update partition to be deselected"
        );
        assert_eq!(
            fs::read_to_string(&omnect_path).expect("read omnect file"),
            "original_arg",
            "expected omnect file to be restored from backup"
        );
        assert!(
            backup_path.exists(),
            "expected backup to be kept for further attempts in this boot"
        );
    }

    #[test]
    fn rollback_without_backup_only_unsets_validate_keys() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        crate::common::set_env_var(
            "UPDATE_VALIDATION_CONFIG_PATH",
            tmp.path().join("update_validation_conf.json"),
        );
        setup_validation_env();

        rollback(&mut make_guard(true, false, None));

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra bootargs"),
            "old_value",
            "expected omnect_extra_bootargs to remain unchanged"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate extra bootargs"),
            "",
            "expected validate key to be unset"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_UPDATE_PART).expect("get validate update part"),
            "",
            "expected update partition to be deselected"
        );
    }

    #[test]
    fn rollback_failed_deselect_of_update_part_keeps_bootargs() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_rollback_files(&tmp, "original_arg", "custom_arg");
        fs::write(&omnect_path, "new_arg").expect("write omnect file");
        let config_path = tmp.path().join("update_validation_conf.json");
        fs::write(&config_path, r#"{"local":true}"#).expect("write config");
        crate::common::set_env_var("UPDATE_VALIDATION_CONFIG_PATH", &config_path);
        setup_validation_env();
        crate::bootloader_env::fail_mock(
            crate::bootloader_env::MockOp::Unset,
            OMNECT_VALIDATE_UPDATE_PART,
        );

        assert!(
            rollback(&mut make_guard(true, false, Some(backup_path))),
            "expected the update partition to count as selected"
        );

        assert!(
            config_path.exists(),
            "expected the validation config to stay while the update partition is selected"
        );

        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate extra bootargs"),
            "staged",
            "expected the new bootargs to stay while the update partition is selected"
        );
        assert_eq!(
            fs::read_to_string(&omnect_path).expect("read omnect file"),
            "new_arg",
            "expected the new omnect file to stay while the update partition is selected"
        );
    }

    #[test]
    fn rollback_failed_unset_of_validate_bootargs_still_restores_bootargs_file() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_rollback_files(&tmp, "original_arg", "custom_arg");
        fs::write(&omnect_path, "new_arg").expect("write omnect file");
        let config_path = tmp.path().join("update_validation_conf.json");
        fs::write(&config_path, r#"{"local":true}"#).expect("write config");
        crate::common::set_env_var("UPDATE_VALIDATION_CONFIG_PATH", &config_path);
        setup_validation_env();
        crate::bootloader_env::fail_mock(
            crate::bootloader_env::MockOp::Unset,
            OMNECT_VALIDATE_EXTRA_BOOTARGS,
        );

        rollback(&mut make_guard(true, false, Some(backup_path)));

        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_UPDATE_PART).expect("get validate update part"),
            "",
            "expected update partition to be deselected"
        );
        assert!(
            !config_path.exists(),
            "expected the persistent config to be removed once the update partition is deselected"
        );
        assert_eq!(
            fs::read_to_string(&omnect_path).expect("read omnect file"),
            "original_arg",
            "expected the omnect file to be restored from backup"
        );
    }

    #[test]
    fn rollback_does_not_create_a_missing_omnect_file() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_rollback_files(&tmp, "", "");
        fs::remove_file(&omnect_path).expect("remove omnect file");
        setup_validation_env();

        rollback(&mut make_guard(true, false, Some(backup_path)));

        assert!(
            !omnect_path.exists(),
            "expected the missing omnect file not to be created"
        );
    }

    #[test]
    fn rollback_removes_update_validation_config() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");

        let config_path = tmp.path().join("update_validation_conf.json");
        fs::write(&config_path, r#"{"local":true}"#).expect("write config");
        crate::common::set_env_var("UPDATE_VALIDATION_CONFIG_PATH", &config_path);

        rollback(&mut make_guard(true, false, None));

        assert!(
            !config_path.exists(),
            "expected update validation config to be removed by rollback"
        );
    }

    #[test]
    fn switch_to_update_part_writes_no_validation_config() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, _) = setup_bootloader_updated_files(&tmp);
        let config_path = tmp.path().join("update_validation_conf.json");
        crate::common::set_env_var("UPDATE_VALIDATION_CONFIG_PATH", &config_path);

        FirmwareUpdate::switch_to_update_part(RootPartition::B).expect("switch to update part");

        assert!(
            !config_path.exists(),
            "expected no update validation config without validation"
        );
        assert_switched_to_new_rootfs(&omnect_path);
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_UPDATE_PART).expect("get validate part"),
            ""
        );
    }

    #[test]
    fn failed_flag_in_switch_to_update_part_still_writes_new_bootargs() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        setup_bootloader_updated_files(&tmp);
        crate::bootloader_env::fail_mock(
            crate::bootloader_env::MockOp::Set,
            OMNECT_BOOTLOADER_UPDATED,
        );

        FirmwareUpdate::switch_to_update_part(RootPartition::B)
            .expect_err("expected the failed flag to be reported");

        assert_eq!(
            bootloader_env::get(OMNECT_OS_BOOTPART).expect("get bootpart"),
            RootPartition::B.index().to_string()
        );
        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra bootargs"),
            "new_arg custom_arg",
            "expected the args to follow the selected partition"
        );
    }

    #[test]
    fn set_boot_partition_without_bootloader_update_writes_validation_config() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let config_path = tmp.path().join("update_validation_conf.json");
        crate::common::set_env_var("UPDATE_VALIDATION_CONFIG_PATH", &config_path);

        FirmwareUpdate::set_boot_partition(RootPartition::B, false).expect("set boot partition");

        let config: UpdateValidationConfig =
            from_json_file(&config_path).expect("read update validation config");
        assert!(config.local);
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_UPDATE_PART).expect("get validate part"),
            RootPartition::B.index().to_string()
        );
        assert_eq!(
            bootloader_env::get(OMNECT_BOOTLOADER_UPDATED).expect("get bootloader updated"),
            ""
        );
    }

    fn setup_bootloader_updated_files(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let (omnect_path, backup_path) = setup_rollback_files(tmp, "original_arg", "custom_arg");
        fs::write(&omnect_path, "new_arg").expect("write omnect file");
        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "original_arg custom_arg")
            .expect("set extra bootargs");
        (omnect_path, backup_path)
    }

    fn assert_switched_to_new_rootfs(omnect_path: &Path) {
        assert_eq!(
            bootloader_env::get(OMNECT_OS_BOOTPART).expect("get bootpart"),
            RootPartition::B.index().to_string()
        );
        assert_eq!(
            bootloader_env::get(OMNECT_BOOTLOADER_UPDATED).expect("get bootloader updated"),
            BOOTLOADER_UPDATED_FLAG
        );
        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra bootargs"),
            "new_arg custom_arg"
        );
        assert_eq!(
            fs::read_to_string(omnect_path).expect("read omnect file"),
            "new_arg"
        );
    }

    // a panic before finalize() drops the guard, whose rollback needs a runtime
    #[tokio::test]
    async fn activate_update_with_bootloader_update_keeps_new_bootargs() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_bootloader_updated_files(&tmp);
        let mut guard = make_guard(false, true, Some(backup_path));

        FirmwareUpdate::activate_update(&mut guard, false).expect("activate update");

        assert!(guard.succeeded, "expected no rollback after the switch");
        assert_switched_to_new_rootfs(&omnect_path);
    }

    #[test]
    fn rollback_after_bootloader_update_switches_to_new_rootfs() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_bootloader_updated_files(&tmp);

        assert!(
            rollback(&mut make_guard(true, true, Some(backup_path))),
            "expected the update partition to be selected"
        );

        assert_switched_to_new_rootfs(&omnect_path);
    }

    // a panic before finalize() drops the guard, whose rollback needs a runtime
    #[tokio::test]
    async fn failed_boot_partition_switch_after_bootloader_update_is_completed_by_rollback() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_bootloader_updated_files(&tmp);
        let mut guard = make_guard(false, true, Some(backup_path));
        crate::bootloader_env::fail_mock(crate::bootloader_env::MockOp::Set, OMNECT_OS_BOOTPART);

        FirmwareUpdate::activate_update(&mut guard, false)
            .expect_err("expected switch of the boot partition to fail");

        assert!(!guard.succeeded, "expected a rollback");
        assert_eq!(
            bootloader_env::get(OMNECT_OS_BOOTPART).expect("get bootpart"),
            "",
            "expected the old partition to stay selected"
        );

        crate::bootloader_env::reset_fail_mock();
        rollback(&mut guard);
        // without finalize(), Drop would start a second rollback that outlives the test lock
        guard.finalize();

        assert_switched_to_new_rootfs(&omnect_path);
    }

    #[test]
    fn failed_switch_in_rollback_after_bootloader_update_keeps_old_bootargs() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_bootloader_updated_files(&tmp);
        crate::bootloader_env::fail_mock(crate::bootloader_env::MockOp::Set, OMNECT_OS_BOOTPART);

        assert!(
            rollback(&mut make_guard(true, true, Some(backup_path))),
            "expected deviceupdate-agent to stay stopped: the new bootloader is installed"
        );

        assert_eq!(
            bootloader_env::get(OMNECT_OS_BOOTPART).expect("get bootpart"),
            ""
        );
        assert_eq!(
            fs::read_to_string(&omnect_path).expect("read omnect file"),
            "new_arg",
            "expected no restore of the bootargs after a bootloader update"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra bootargs"),
            "original_arg custom_arg",
            "expected the old rootfs to stay with its args"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_BOOTLOADER_UPDATED).expect("get bootloader updated"),
            ""
        );
    }

    #[test]
    fn failed_bootargs_in_rollback_after_bootloader_update_still_switches() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (_, backup_path) = setup_bootloader_updated_files(&tmp);
        crate::bootloader_env::fail_mock(crate::bootloader_env::MockOp::Set, OMNECT_EXTRA_BOOTARGS);

        assert!(
            rollback(&mut make_guard(true, true, Some(backup_path))),
            "expected deviceupdate-agent to stay stopped: the new rootfs is selected"
        );

        assert_eq!(
            bootloader_env::get(OMNECT_OS_BOOTPART).expect("get bootpart"),
            RootPartition::B.index().to_string(),
            "expected the new rootfs to be selected"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra bootargs"),
            "original_arg custom_arg"
        );
    }

    #[test]
    fn pending_switch_after_bootloader_update_blocks_run() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("omnect-bootloader-update");
        crate::common::set_env_var("BOOTLOADER_UPDATE_PATH", &marker);

        FirmwareUpdate::ensure_no_pending_switch(RootPartition::B)
            .expect("expected no pending switch without marker and bootpart");

        bootloader_env::set(OMNECT_OS_BOOTPART, &RootPartition::A.index().to_string())
            .expect("set bootpart");
        FirmwareUpdate::ensure_no_pending_switch(RootPartition::B)
            .expect("expected no pending switch while the current partition is selected");

        fs::write(&marker, "").expect("write marker");
        crate::bootloader_env::fail_mock(crate::bootloader_env::MockOp::Set, OMNECT_OS_BOOTPART);
        let err = FirmwareUpdate::ensure_no_pending_switch(RootPartition::B)
            .expect_err("expected the bootloader marker to block the run");
        assert!(
            err.to_string().contains("do not reboot"),
            "expected a warning not to reboot while the current rootfs is selected: {err}"
        );

        crate::bootloader_env::reset_fail_mock();
        let omnect_path = setup_bootloader_updated_files(&tmp).0;
        let err = FirmwareUpdate::ensure_no_pending_switch(RootPartition::B)
            .expect_err("expected the bootloader marker to block the run");
        assert!(
            err.to_string().contains("reboot before"),
            "expected advice to reboot: {err}"
        );
        assert_switched_to_new_rootfs(&omnect_path);

        fs::remove_file(&marker).expect("remove marker");
        crate::bootloader_env::clear_mock();
        bootloader_env::set(OMNECT_OS_BOOTPART, &RootPartition::B.index().to_string())
            .expect("set bootpart");
        let err = FirmwareUpdate::ensure_no_pending_switch(RootPartition::B)
            .expect_err("expected the selected update partition to block the run");
        // during the trial boot of a validated update the saved bootpart also equals the target,
        // and a reboot there would roll that update back
        assert!(
            !err.to_string().contains("reboot"),
            "expected no advice to reboot: {err}"
        );

        bootloader_env::set(OMNECT_OS_BOOTPART, &RootPartition::A.index().to_string())
            .expect("set bootpart");
        bootloader_env::set(
            OMNECT_VALIDATE_UPDATE_PART,
            &RootPartition::B.index().to_string(),
        )
        .expect("set validate update part");
        let err = FirmwareUpdate::ensure_no_pending_switch(RootPartition::B)
            .expect_err("expected a pending validated switch to block the run");
        assert!(
            err.to_string().contains("reboot before"),
            "expected advice to reboot: {err}"
        );
    }

    #[test]
    fn failed_marker_check_blocks_run_without_switching() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let not_a_dir = tmp.path().join("file");
        fs::write(&not_a_dir, "").expect("write file");
        crate::common::set_env_var("BOOTLOADER_UPDATE_PATH", not_a_dir.join("marker"));
        bootloader_env::set(OMNECT_OS_BOOTPART, &RootPartition::A.index().to_string())
            .expect("set bootpart");

        let err = FirmwareUpdate::ensure_no_pending_switch(RootPartition::B)
            .expect_err("expected a failed marker check to block the run");

        assert!(
            err.to_string().contains("failed to check"),
            "expected the failed check to be reported: {err}"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_OS_BOOTPART).expect("get bootpart"),
            RootPartition::A.index().to_string(),
            "expected no switch"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_BOOTLOADER_UPDATED).unwrap_or_default(),
            "",
            "expected the bootloader updated flag to stay unset"
        );
    }

    #[test]
    fn bootloader_markers_detect_a_failed_write() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let updated_marker = tmp.path().join("omnect-bootloader-update");
        let not_necessary_marker = tmp.path().join("omnect-bootloader-update-not-necessary");
        crate::common::set_env_var("BOOTLOADER_UPDATE_PATH", &updated_marker);
        crate::common::set_env_var("NO_BOOTLOADER_UPDATE_PATH", &not_necessary_marker);

        FirmwareUpdate::ensure_bootloader_markers(
            false,
            FirmwareUpdate::marker_exists(&bootloader_updated_file_path!()),
        )
        .expect_err("expected a missing not-necessary marker to fail");
        FirmwareUpdate::ensure_bootloader_markers(
            true,
            FirmwareUpdate::marker_exists(&bootloader_updated_file_path!()),
        )
        .expect("expected a successful swupdate to count as a bootloader update");

        fs::write(&not_necessary_marker, "").expect("write not-necessary marker");
        FirmwareUpdate::ensure_bootloader_markers(
            false,
            FirmwareUpdate::marker_exists(&bootloader_updated_file_path!()),
        )
        .expect("expected no bootloader update to be necessary");

        fs::write(&updated_marker, "").expect("write bootloader marker");
        FirmwareUpdate::ensure_bootloader_markers(
            false,
            FirmwareUpdate::marker_exists(&bootloader_updated_file_path!()),
        )
        .expect_err("expected the bootloader marker to reveal the failed write");
        FirmwareUpdate::ensure_bootloader_markers(
            true,
            FirmwareUpdate::marker_exists(&bootloader_updated_file_path!()),
        )
        .expect("expected a bootloader update");
    }

    #[test]
    fn bootloader_step_removes_a_stale_not_necessary_marker_first() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let updated_marker = tmp.path().join("omnect-bootloader-update");
        let not_necessary_marker = tmp.path().join("omnect-bootloader-update-not-necessary");
        crate::common::set_env_var("BOOTLOADER_UPDATE_PATH", &updated_marker);
        crate::common::set_env_var("NO_BOOTLOADER_UPDATE_PATH", &not_necessary_marker);
        let mut guard = make_guard(true, false, None);

        fs::write(&not_necessary_marker, "").expect("write stale not-necessary marker");
        FirmwareUpdate::bootloader_step(&mut guard, || bail!("bootloader swupdate failed"))
            .expect_err("expected the removed stale marker to reveal the failed step");

        FirmwareUpdate::bootloader_step(&mut guard, || {
            fs::write(&not_necessary_marker, "")?;
            bail!("no bootloader update necessary")
        })
        .expect("expected the not-necessary marker of this step to be kept");

        fs::remove_file(&not_necessary_marker).expect("remove not-necessary marker");
        fs::create_dir(&not_necessary_marker).expect("create undeletable not-necessary marker");
        let mut swupdate_called = false;
        FirmwareUpdate::bootloader_step(&mut guard, || {
            swupdate_called = true;
            Ok(())
        })
        .expect_err("expected a not-necessary marker that cannot be removed to abort");
        assert!(!swupdate_called, "expected no bootloader swupdate");
    }

    #[test]
    fn bootloader_step_reports_a_failed_marker_check() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let not_a_dir = tmp.path().join("file");
        fs::write(&not_a_dir, "").expect("write file");
        crate::common::set_env_var("BOOTLOADER_UPDATE_PATH", not_a_dir.join("marker"));
        crate::common::set_env_var(
            "NO_BOOTLOADER_UPDATE_PATH",
            tmp.path().join("omnect-bootloader-update-not-necessary"),
        );
        let mut guard = make_guard(true, false, None);

        FirmwareUpdate::bootloader_step(&mut guard, || Ok(()))
            .expect("expected a successful swupdate to count as a bootloader update");
        assert!(guard.bootloader_updated, "expected a bootloader update");

        let err =
            FirmwareUpdate::bootloader_step(&mut guard, || bail!("bootloader swupdate failed"))
                .expect_err("expected a failed marker check to fail the step");
        assert!(
            err.to_string().contains("cannot tell"),
            "expected the unknown bootloader state to be reported: {err}"
        );
    }

    #[test]
    fn failed_not_necessary_marker_check_is_reported() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let not_a_dir = tmp.path().join("file");
        fs::write(&not_a_dir, "").expect("write file");
        crate::common::set_env_var(
            "BOOTLOADER_UPDATE_PATH",
            tmp.path().join("omnect-bootloader-update"),
        );
        crate::common::set_env_var("NO_BOOTLOADER_UPDATE_PATH", not_a_dir.join("marker"));

        let err = FirmwareUpdate::ensure_bootloader_markers(false, Ok(false))
            .expect_err("expected a failed not-necessary marker check to fail");
        assert!(
            err.to_string()
                .contains("cannot tell whether a bootloader update was necessary"),
            "expected the unknown necessity to be reported: {err}"
        );
    }

    #[test]
    fn bootloader_marker_decides_whether_rollback_completes_the_switch() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_bootloader_updated_files(&tmp);
        let updated_marker = tmp.path().join("omnect-bootloader-update");
        let not_necessary_marker = tmp.path().join("omnect-bootloader-update-not-necessary");
        crate::common::set_env_var("BOOTLOADER_UPDATE_PATH", &updated_marker);
        crate::common::set_env_var("NO_BOOTLOADER_UPDATE_PATH", &not_necessary_marker);

        // swupdate succeeded, but the hook never created the marker: the bootloader was written
        // anyway, so the activation switches without validation
        let mut guard = make_guard(true, false, Some(backup_path.clone()));
        FirmwareUpdate::bootloader_step(&mut guard, || Ok(()))
            .expect("expected a successful swupdate to count as a bootloader update");
        assert!(guard.bootloader_updated, "expected a bootloader update");
        FirmwareUpdate::activate_update(&mut guard, false).expect("activate update");
        assert_switched_to_new_rootfs(&omnect_path);

        // swupdate failed after the hook created the marker: the marker is removed and the rollback
        // reverts
        crate::bootloader_env::clear_mock();
        fs::write(&omnect_path, "new_arg").expect("write omnect file");
        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "original_arg custom_arg")
            .expect("set extra bootargs");
        let mut guard = make_guard(true, false, Some(backup_path.clone()));
        FirmwareUpdate::bootloader_step(&mut guard, || {
            fs::write(&updated_marker, "")?;
            bail!("bootloader write failed")
        })
        .expect_err("expected the bootloader marker to reveal the failed write");
        assert!(
            !updated_marker.exists(),
            "expected the marker to be removed"
        );
        assert!(!guard.bootloader_updated, "expected no bootloader update");
        setup_validation_env();
        assert!(!rollback(&mut guard), "expected a revert");
        assert_eq!(
            fs::read_to_string(&omnect_path).expect("read omnect file"),
            "original_arg"
        );

        // the marker cannot be removed: it stays authoritative, so the rollback switches
        crate::bootloader_env::clear_mock();
        fs::write(&omnect_path, "new_arg").expect("write omnect file");
        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "original_arg custom_arg")
            .expect("set extra bootargs");
        let mut guard = make_guard(true, false, Some(backup_path));
        FirmwareUpdate::bootloader_step(&mut guard, || {
            fs::create_dir(&updated_marker)?;
            bail!("bootloader write failed")
        })
        .expect_err("expected the bootloader marker to reveal the failed write");
        assert!(guard.bootloader_updated, "expected a bootloader update");
        assert!(
            rollback(&mut guard),
            "expected the switch to be completed with the marker"
        );
        assert_switched_to_new_rootfs(&omnect_path);
    }

    #[test]
    fn backup_bootargs_once_keeps_existing_backup() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_rollback_files(&tmp, "original_arg", "");
        fs::write(&omnect_path, "new_arg").expect("write omnect file");

        let backup = FirmwareUpdate::backup_bootargs_once().expect("back up bootargs");

        assert_eq!(backup, backup_path);
        assert_eq!(
            fs::read_to_string(&backup_path).expect("read backup"),
            "original_arg"
        );
    }

    #[test]
    fn backup_bootargs_once_backs_up_missing_omnect_file_as_empty() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        let tmp = tempfile::tempdir().expect("tempdir");
        let omnect_path = tmp.path().join("bootargs_omnect");
        let backup_path = tmp.path().join("bootargs_omnect.backup");
        crate::common::set_env_var("BOOTARGS_OMNECT_FILE_PATH", &omnect_path);
        crate::common::set_env_var("BOOTARGS_OMNECT_BACKUP_FILE_PATH", &backup_path);

        FirmwareUpdate::backup_bootargs_once().expect("back up bootargs");

        assert!(
            !omnect_path.exists(),
            "expected no omnect file to be created before the first swupdate"
        );
        assert_eq!(fs::read_to_string(&backup_path).expect("read backup"), "");
    }

    #[test]
    fn drop_without_finalize_triggers_rollback() {
        let _lock = BOOTARGS_TEST_LOCK.lock().expect("bootargs test lock");
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (omnect_path, backup_path) = setup_rollback_files(&tmp, "original_arg", "custom_arg");

        let config_path = tmp.path().join("update_validation_conf.json");
        fs::write(&config_path, r#"{"local":true}"#).expect("write config");
        crate::common::set_env_var("UPDATE_VALIDATION_CONFIG_PATH", &config_path);

        // simulate kernelargs swupdate overwriting the omnect file
        fs::write(&omnect_path, "corrupted").expect("write omnect file");
        setup_validation_env();

        // Drop delegates to do_rollback — call rollback() directly to test
        // the same code path without relying on async timing.
        rollback(&mut make_guard(true, false, Some(backup_path)));

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra bootargs"),
            "old_value"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate extra bootargs"),
            "",
            "expected validate key to be unset"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_UPDATE_PART).expect("get validate update part"),
            "",
            "expected update partition to be deselected"
        );
        assert_eq!(
            fs::read_to_string(&omnect_path).expect("read omnect file"),
            "original_arg",
            "expected omnect file to be restored from backup"
        );
        assert!(
            !config_path.exists(),
            "expected update validation config to be removed"
        );
    }
}
