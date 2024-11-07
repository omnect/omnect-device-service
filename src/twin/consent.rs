use super::{feature::*, Feature};
use crate::consent_path;
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{info, warn};
use notify_debouncer_full::{notify::*, Debouncer, NoCache};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    env,
    fs::OpenOptions,
    path::{Path, PathBuf},
};
use tokio::sync::mpsc::Sender;

#[macro_export]
macro_rules! consent_path {
    () => {{
        static CONSENT_DIR_PATH_DEFAULT: &'static str = "/etc/omnect/consent";
        std::env::var("CONSENT_DIR_PATH").unwrap_or(CONSENT_DIR_PATH_DEFAULT.to_string())
    }};
}

macro_rules! request_consent_path {
    () => {{
        PathBuf::from(&format!(r"{}/request_consent.json", consent_path!()))
    }};
}

#[macro_export]
macro_rules! history_consent_path {
    () => {{
        PathBuf::from(&format!(r"{}/history_consent.json", consent_path!()))
    }};
}

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct UserConsentCommand {
    pub user_consent: HashMap<String, String>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct DesiredGeneralConsentCommand {
    pub general_consent: Vec<String>,
}

#[derive(Default)]
pub struct DeviceUpdateConsent {
    file_observer: Option<Debouncer<INotifyWatcher, NoCache>>,
    tx_reported_properties: Option<Sender<serde_json::Value>>,
}

#[async_trait(?Send)]
impl Feature for DeviceUpdateConsent {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::USER_CONSENT_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_DEVICE_UPDATE_USER_CONSENT") != Ok("true".to_string())
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        _tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.tx_reported_properties = Some(tx_reported_properties.clone());

        self.report_general_consent().await?;
        self.report_user_consent(&request_consent_path!()).await?;
        self.report_user_consent(&history_consent_path!()).await
    }

    fn event_stream(&mut self) -> Result<Option<EventStream>> {
        let (file_observer, stream) = file_modified_stream::<DeviceUpdateConsent>(vec![
            request_consent_path!().as_path(),
            history_consent_path!().as_path(),
        ])
        .context("refresh_event: cannot create file_modified_stream")?;
        self.file_observer = Some(file_observer);
        Ok(Some(stream))
    }

    async fn command(&mut self, cmd: Command) -> CommandResult {
        match cmd {
            Command::FileModified(file) => {
                self.report_user_consent(&file.path).await?;
            }
            Command::DesiredGeneralConsent(cmd) => {
                self.update_general_consent(cmd).await?;
            }
            Command::UserConsent(cmd) => {
                self.user_consent(cmd)?;
            }
            _ => bail!("unexpected command"),
        };

        Ok(None)
    }
}

impl DeviceUpdateConsent {
    const USER_CONSENT_VERSION: u8 = 1;
    const ID: &'static str = "device_update_consent";

    fn user_consent(&self, cmd: UserConsentCommand) -> CommandResult {
        info!("user consent requested: {cmd:?}");

        for (component, version) in cmd.user_consent {
            ensure!(
                !component.contains(std::path::is_separator),
                "user_consent: invalid component name: {component}"
            );

            let path_string = format!("{}/{}/user_consent.json", consent_path!(), component);
            let path = Path::new(&path_string);
            let path_canonicalized = path.canonicalize().context(format!(
                "user_consent: invalid path {}",
                path.to_string_lossy()
            ))?;

            // ensure path is valid and absolute and not a link
            ensure!(
                path_canonicalized
                    .to_string_lossy()
                    .eq(path_string.as_str()),
                "user_consent: non-absolute path {}",
                path.to_string_lossy()
            );

            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(path)
                    .context("user_consent: open user_consent.json for write")?,
                &json!({ "consent": version }),
            )
            .context("user_consent: serde_json::to_writer_pretty")?;
        }

        Ok(None)
    }

    async fn update_general_consent(
        &self,
        desired_consents: DesiredGeneralConsentCommand,
    ) -> CommandResult {
        let mut new_consents = desired_consents.general_consent;
        // enforce entries only exists once
        new_consents.sort_by_key(|name| name.to_string());
        new_consents.dedup();

        let mut current_config: DesiredGeneralConsentCommand = serde_json::from_reader(
            OpenOptions::new()
                .read(true)
                .create(false)
                .open(format!("{}/consent_conf.json", consent_path!()))
                .context("update_general_consent: open consent_conf.json for read")?,
        )
        .context("update_general_consent: serde_json::from_reader")?;

        current_config.general_consent.iter_mut().for_each(|s| {
            *s = s.to_lowercase();
        });

        // check if consents changed (current desired vs. saved)
        if new_consents.eq(&current_config.general_consent) {
            info!("desired general_consent didn't change");
            return Ok(None);
        }

        serde_json::to_writer_pretty(
            OpenOptions::new()
                .write(true)
                .create(false)
                .truncate(true)
                .open(format!("{}/consent_conf.json", consent_path!()))
                .context("update_general_consent: open consent_conf.json for write")?,
            &serde_json::Value::from(json!({"general_consent": new_consents})),
        )
        .context("update_general_consent: serde_json::to_writer_pretty")?;

        self.report_general_consent()
            .await
            .context("update_general_consent: report_general_consent")?;

        Ok(None)
    }

    async fn report_general_consent(&self) -> Result<()> {
        self.report_consent(
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(format!("{}/consent_conf.json", consent_path!()))
                    .context("report_general_consent: open consent_conf.json fo read")?,
            )
            .context("report_general_consent: serde_json::from_reader")?,
        )
        .await
        .context("report_general_consent: report_consent")
    }

    async fn report_user_consent(&self, report_consent_file: &Path) -> Result<()> {
        self.report_consent(
            serde_json::from_reader(
                OpenOptions::new()
                    .read(true)
                    .create(false)
                    .open(report_consent_file)
                    .context("report_user_consent: open report_consent_file for read")?,
            )
            .context("report_user_consent: serde_json::from_reader")?,
        )
        .await
        .context("report_user_consent: report_consent")
    }

    async fn report_consent(&self, value: serde_json::Value) -> Result<()> {
        let Some(tx) = &self.tx_reported_properties else {
            warn!("report_consent: skip since tx_reported_properties is None");
            return Ok(());
        };

        tx.send(json!({ "device_update_consent": value }))
            .await
            .context("report_consent: report_impl")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;

    #[test]
    fn update_and_report_general_consent_test() {
        let (tx_reported_properties, _rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let usr_consent = DeviceUpdateConsent {
            file_observer: None,
            tx_reported_properties: Some(tx_reported_properties),
        };
        /*
        assert!(block_on(async { usr_consent.update_general_consent(None).await }).is_ok());

        let err = block_on(async {
            usr_consent
                .update_general_consent(Some(json!([1, 1]).as_array().unwrap()))
                .await
        })
        .unwrap_err();

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("update_general_consent: parse desired_consents")));

        let err = block_on(async {
            usr_consent
                .update_general_consent(Some(json!(["1", "1"]).as_array().unwrap()))
                .await
        })
        .unwrap_err();

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("update_general_consent: open consent_conf.json"))); */
    }
}
