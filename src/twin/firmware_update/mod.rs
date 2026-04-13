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

struct RunUpdateGuard {
    succeeded: bool,
    wdt: Option<Duration>,
    bootloader_updated: bool,
    bootargs_omnect_backup: Option<PathBuf>,
}

impl RunUpdateGuard {
    async fn new() -> Result<Self> {
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
            bootloader_updated: false,
            bootargs_omnect_backup: None,
        })
    }

    fn finalize(&mut self) {
        self.succeeded = true;
    }

    // rollback only logs error's during it's processing and doesn't return on them
    fn rollback(&mut self) {
        // cannot rollback a stable,bootloader image once flashed
        if self.bootloader_updated {
            warn!("bootloader was updated: rollback not possible");
            return;
        }

        if let Err(e) = bootloader_env::unset(OMNECT_VALIDATE_EXTRA_BOOTARGS) {
            error!("failed to unset {OMNECT_VALIDATE_EXTRA_BOOTARGS}: {e:#}");
        }

        if let Some(backup) = self.bootargs_omnect_backup.take() {
            let omnect_file = bootargs_omnect_file_path!();
            match fs::copy(&backup, &omnect_file) {
                Err(e) => {
                    error!("failed to restore omnect bootargs file from backup: {e:#}");
                }
                Ok(_) => {
                    let omnect_args = fs::read_to_string(&omnect_file).unwrap_or_default();
                    let custom_args =
                        fs::read_to_string(bootargs_custom_file_path!()).unwrap_or_default();
                    let new_bootargs = format!("{omnect_args} {custom_args}")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");

                    let result = if new_bootargs.is_empty() {
                        bootloader_env::unset(OMNECT_EXTRA_BOOTARGS)
                    } else {
                        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, &new_bootargs)
                    };
                    if let Err(e) = result {
                        error!("failed to restore {OMNECT_EXTRA_BOOTARGS}: {e:#}");
                    }
                }
            }
            let _ = fs::remove_file(&backup);
        }

        let _ = fs::remove_file(update_validation_config_path!());
    }
}

impl Drop for RunUpdateGuard {
    fn drop(&mut self) {
        if !(self.succeeded) {
            let wdt = self.wdt.take();

            debug!(
                "run update failed: restore old wdt ({wdt:?}) and restart {IOT_HUB_DEVICE_UPDATE_SERVICE} and {IOT_HUB_DEVICE_UPDATE_SERVICE_TIMER}"
            );

            self.rollback();

            tokio::spawn(async move {
                if let Some(wdt) = wdt
                    && let Err(e) = WatchdogManager::interval(wdt).await
                {
                    error!("failed to restore wdt interval: {e:#}")
                }

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
                manifest_sha1 = format!(
                    "{:X}",
                    Sha256::digest(
                        std::fs::read(&path)
                            .context("failed to read *.swu.importManifest.json for hash")?
                    )
                );
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
        let Some(ref swu_file_path) = self.swu_file_path else {
            bail!("no update loaded")
        };

        let target_partition = RootPartition::current()?.other();

        let mut guard = RunUpdateGuard::new().await?;

        #[cfg(not(feature = "mock"))]
        Self::swupdate(swu_file_path, target_partition.root_update_params()).context(format!(
            "failed to update root partition: swupdate logs at {}",
            log_file_path!()
        ))?;

        let _ = fs::remove_file(no_bootloader_updated_file_path!());

        let bootloader_updated =
            Self::swupdate(swu_file_path, target_partition.bootloader_update_params()).is_ok();

        let _expected_file = if bootloader_updated {
            bootloader_updated_file_path!()
        } else {
            no_bootloader_updated_file_path!()
        };

        #[cfg(not(feature = "mock"))]
        ensure!(
            Path::new(&_expected_file).try_exists().is_ok_and(|r| r),
            "failed to update bootloader: expected {} to be present. (swupdate logs at {})",
            _expected_file,
            log_file_path!()
        );

        guard.bootloader_updated = bootloader_updated;

        let omnect_file = bootargs_omnect_file_path!();
        let backup_file = PathBuf::from(bootargs_omnect_backup_file_path!());
        if Path::new(&omnect_file).exists() {
            fs::copy(&omnect_file, &backup_file)
                .context("failed to back up omnect bootargs file")?;
            guard.bootargs_omnect_backup = Some(backup_file);
        } else {
            error!("omnect bootargs file not found: {omnect_file}, skipping backup");
        }

        #[cfg(not(feature = "mock"))]
        Self::swupdate(swu_file_path, target_partition.kernelargs_update_params()).context(
            format!(
                "failed to update kernelargs: swupdate logs at {}",
                log_file_path!()
            ),
        )?;

        Self::apply_bootargs(bootloader_updated)?;

        to_json_file(
            &UpdateValidationConfig {
                local: !validate_iothub_connection,
            },
            update_validation_config_path!(),
            true,
        )?;

        if bootloader_updated {
            bootloader_env::set(OMNECT_BOOTLOADER_UPDATED, "1")?;
            bootloader_env::set(OMNECT_OS_BOOTPART, &target_partition.index().to_string())?;
        } else {
            bootloader_env::set(
                OMNECT_VALIDATE_UPDATE_PART,
                &target_partition.index().to_string(),
            )?;
        }

        // explicitly finalize even if reboot fails
        guard.finalize();

        systemd::reboot("swupdate", "local update").await?;

        info!("update succeeded");

        Ok(None)
    }

    fn apply_bootargs(bootloader_updated: bool) -> Result<()> {
        let current_bootargs = bootloader_env::get(OMNECT_EXTRA_BOOTARGS).unwrap_or_default();
        let omnect_bootargs = fs::read_to_string(bootargs_omnect_file_path!())?; // has to exist
        let custom_bootargs = match fs::read_to_string(bootargs_custom_file_path!()) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(), // optional: missing file
            Err(e) => return Err(e.into()),
        };
        let new_bootargs = format!("{} {}", omnect_bootargs, custom_bootargs)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        if current_bootargs != new_bootargs {
            if bootloader_updated && new_bootargs.is_empty() {
                bootloader_env::unset(OMNECT_EXTRA_BOOTARGS)?;
            } else if bootloader_updated {
                bootloader_env::set(OMNECT_EXTRA_BOOTARGS, &new_bootargs)?;
            } else if new_bootargs.is_empty() {
                bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, NOARGS_SENTINEL)?;
            } else {
                bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, &new_bootargs)?;
            }
        }
        Ok(())
    }

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
            std::process::Command::new("sudo")
                .arg("-u")
                .arg("adu")
                .arg("swupdate")
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
    use std::sync::Mutex;
    use tempfile;

    // Serializes all bootargs tests because both set_env_var (for file paths)
    // and the bootloader_env mock store are global state.
    static BOOTARGS_TEST_LOCK: Mutex<()> = Mutex::new(());

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
        bootloader_updated: bool,
        bootargs_omnect_backup: Option<PathBuf>,
    ) -> RunUpdateGuard {
        RunUpdateGuard {
            succeeded: true, // prevent Drop from running async cleanup
            wdt: None,
            bootloader_updated,
            bootargs_omnect_backup,
        }
    }

    fn setup_rollback_files(
        tmp: &tempfile::TempDir,
        omnect: &str,
        custom: &str,
    ) -> (PathBuf, PathBuf) {
        let omnect_path = tmp.path().join("bootargs_omnect");
        let custom_path = tmp.path().join("bootargs_custom");
        let backup_path = tmp.path().join("bootargs_omnect.backup");
        fs::write(&omnect_path, omnect).expect("write omnect");
        fs::write(&custom_path, custom).expect("write custom");
        fs::write(&backup_path, omnect).expect("write backup");
        crate::common::set_env_var("BOOTARGS_OMNECT_FILE_PATH", &omnect_path);
        crate::common::set_env_var("BOOTARGS_CUSTOM_FILE_PATH", &custom_path);
        crate::common::set_env_var("BOOTARGS_OMNECT_BACKUP_FILE_PATH", &backup_path);
        (omnect_path, backup_path)
    }

    #[test]
    fn rollback_restores_bootargs_from_backup() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();
        let (omnect_path, backup_path) = setup_rollback_files(&tmp, "original_arg", "custom_arg");

        // simulate kernelargs swupdate overwriting the omnect file
        fs::write(&omnect_path, "corrupted").expect("overwrite omnect");
        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").expect("set extra");
        bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, "staged").expect("set validate");

        make_guard(false, Some(backup_path.clone())).rollback();

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra"),
            "original_arg custom_arg"
        );
        assert!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate")
                .is_empty(),
            "expected validate key to be unset"
        );
        assert_eq!(
            fs::read_to_string(&omnect_path).expect("read omnect"),
            "original_arg",
            "expected omnect file to be restored from backup"
        );
        assert!(
            !backup_path.exists(),
            "expected backup file to be cleaned up"
        );
    }

    #[test]
    fn rollback_empty_bootargs_unsets_env_var() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();
        let (_omnect_path, backup_path) = setup_rollback_files(&tmp, "", "");

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "old_value").expect("set extra");

        make_guard(false, Some(backup_path)).rollback();

        assert!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS)
                .expect("get extra")
                .is_empty(),
            "expected omnect_extra_bootargs to be unset"
        );
    }

    #[test]
    fn rollback_without_backup_only_unsets_validate_key() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "unchanged").expect("set extra");
        bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, "staged").expect("set validate");

        make_guard(false, None).rollback();

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra"),
            "unchanged",
            "expected omnect_extra_bootargs to remain unchanged"
        );
        assert!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS)
                .expect("get validate")
                .is_empty(),
            "expected validate key to be unset"
        );
    }

    #[test]
    fn rollback_bootloader_updated_does_nothing() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();

        bootloader_env::set(OMNECT_EXTRA_BOOTARGS, "unchanged").expect("set extra");
        bootloader_env::set(OMNECT_VALIDATE_EXTRA_BOOTARGS, "staged").expect("set validate");

        make_guard(true, None).rollback();

        assert_eq!(
            bootloader_env::get(OMNECT_EXTRA_BOOTARGS).expect("get extra"),
            "unchanged"
        );
        assert_eq!(
            bootloader_env::get(OMNECT_VALIDATE_EXTRA_BOOTARGS).expect("get validate"),
            "staged",
            "expected validate key to remain when bootloader was updated"
        );
    }

    #[test]
    fn rollback_removes_update_validation_config() {
        let _lock = BOOTARGS_TEST_LOCK.lock().unwrap();
        crate::bootloader_env::clear_mock();
        let tmp = tempfile::tempdir().unwrap();

        let config_path = tmp.path().join("update_validation_conf.json");
        fs::write(&config_path, r#"{"local":true}"#).expect("write config");
        crate::common::set_env_var("UPDATE_VALIDATION_CONFIG_PATH", &config_path);

        make_guard(false, None).rollback();

        assert!(
            !config_path.exists(),
            "expected update validation config to be removed by rollback"
        );
    }
}
