use crate::{
    common::{from_json_file, to_json_file},
    twin::{Feature, feature::*},
};
use anyhow::{Context, Result, bail, ensure};
use azure_iot_sdk::client::IotMessage;
use log::{info, warn};
use notify_debouncer_full::{Debouncer, NoCache, notify::*};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, env, path::Path};
use tokio::sync::mpsc::Sender;

macro_rules! consent_path {
    () => {
        Path::new(&env::var("CONSENT_DIR_PATH").unwrap_or("/etc/omnect/consent".to_string()))
    };
}

macro_rules! general_consent_path {
    () => {
        consent_path!().join("consent_conf.json")
    };
}

macro_rules! request_consent_path {
    () => {
        consent_path!().join("request_consent.json")
    };
}

macro_rules! history_consent_path {
    () => {
        consent_path!().join("history_consent.json")
    };
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct UserConsentCommand {
    pub user_consent: HashMap<String, String>,
}

type GeneralConsent = Vec<String>;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct DesiredGeneralConsentCommand {
    pub general_consent: GeneralConsent,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct ConsentConfig {
    general_consent: GeneralConsent,
    reset_consent_on_fail: bool,
}

#[derive(Default)]
pub struct DeviceUpdateConsent {
    file_observer: Option<Debouncer<INotifyWatcher, NoCache>>,
    tx_reported_properties: Option<Sender<serde_json::Value>>,
}

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

        self.report_consent(from_json_file(general_consent_path!())?)
            .await?;
        self.report_consent(from_json_file(request_consent_path!())?)
            .await?;
        self.report_consent(from_json_file(history_consent_path!())?)
            .await
    }

    fn command_request_stream(&mut self) -> CommandRequestStreamResult {
        let (file_observer, stream) = file_modified_stream::<DeviceUpdateConsent>(vec![
            request_consent_path!().as_path(),
            history_consent_path!().as_path(),
        ])
        .context("command_request_stream: cannot create file_modified_stream")?;
        self.file_observer = Some(file_observer);
        Ok(Some(stream))
    }

    async fn command(&mut self, cmd: &Command) -> CommandResult {
        match cmd {
            Command::FileModified(file) => {
                self.report_consent(from_json_file(&file.path)?).await?;
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

    fn user_consent(&self, cmd: &UserConsentCommand) -> CommandResult {
        info!("user consent requested: {cmd:?}");

        for (component, version) in &cmd.user_consent {
            ensure!(
                !component.contains(std::path::is_separator),
                "user_consent: invalid component name: {component}"
            );

            to_json_file(
                &json!({ "consent": version }),
                consent_path!().join(component).join("user_consent.json"),
                false,
            )?;
        }

        Ok(None)
    }

    async fn update_general_consent(
        &self,
        desired_consents: &DesiredGeneralConsentCommand,
    ) -> CommandResult {
        let mut new_consents = desired_consents.general_consent.clone();
        // enforce entries only exists once
        new_consents.sort_by_key(|name| name.to_string());
        new_consents.dedup();

        let mut current_config: ConsentConfig = from_json_file(general_consent_path!())?;

        current_config.general_consent.iter_mut().for_each(|s| {
            *s = s.to_lowercase();
        });

        // check if consents changed (current desired vs. saved)
        if new_consents.eq(&current_config.general_consent) {
            info!("desired general_consent didn't change");
            return Ok(None);
        }

        current_config.general_consent = new_consents;

        to_json_file(&current_config, general_consent_path!(), false)?;

        self.report_consent(serde_json::to_value(current_config)?)
            .await?;

        Ok(None)
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
    use std::any::TypeId;

    use super::*;
    use tempfile;

    #[tokio::test(flavor = "multi_thread")]
    async fn consent_files_changed_test() {
        let (tx_reported_properties, mut rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let mut consent = DeviceUpdateConsent {
            file_observer: None,
            tx_reported_properties: Some(tx_reported_properties),
        };

        assert!(
            consent
                .command(&Command::FileModified(PathCommand {
                    feature_id: TypeId::of::<DeviceUpdateConsent>(),
                    path: Path::new("my-path").to_path_buf(),
                }))
                .await
                .unwrap_err()
                .chain()
                .any(|e| e.to_string().starts_with("failed to open for read: "))
        );

        consent
            .command(&Command::FileModified(PathCommand {
                feature_id: TypeId::of::<DeviceUpdateConsent>(),
                path: Path::new("testfiles/positive/test_component/user_consent.json")
                    .to_path_buf(),
            }))
            .await
            .unwrap();

        assert_eq!(
            rx_reported_properties.recv().await,
            Some(json!({"device_update_consent": {}}))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn desired_consent_test() {
        let (tx_reported_properties, mut rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let mut consent = DeviceUpdateConsent {
            file_observer: None,
            tx_reported_properties: Some(tx_reported_properties),
        };

        assert!(
            consent
                .command(&Command::DesiredGeneralConsent(
                    DesiredGeneralConsentCommand {
                        general_consent: vec![],
                    },
                ))
                .await
                .unwrap_err()
                .chain()
                .any(|e| e.to_string().starts_with("failed to open for read: "))
        );

        let tmp_dir = tempfile::tempdir().unwrap();
        let consent_conf_file = tmp_dir.path().join("consent_conf.json");
        crate::common::set_env_var("CONSENT_DIR_PATH", tmp_dir.path());

        std::fs::copy("testfiles/negative/consent_conf.json", &consent_conf_file).unwrap();

        assert!(
            consent
                .command(&Command::DesiredGeneralConsent(
                    DesiredGeneralConsentCommand {
                        general_consent: vec![],
                    },
                ))
                .await
                .unwrap_err()
                .chain()
                .any(|e| e
                    .to_string()
                    .starts_with("failed to deserialize json from: "))
        );

        std::fs::copy("testfiles/positive/consent_conf.json", &consent_conf_file).unwrap();

        consent
            .command(&Command::DesiredGeneralConsent(
                DesiredGeneralConsentCommand {
                    general_consent: vec![],
                },
            ))
            .await
            .unwrap();

        assert_eq!(
            rx_reported_properties.recv().await,
            Some(json!({
            "device_update_consent": {
                "general_consent": [],
                "reset_consent_on_fail": false
            }}))
        );

        let consent_conf: ConsentConfig = from_json_file(&consent_conf_file).unwrap();

        assert_eq!(
            consent_conf,
            ConsentConfig {
                general_consent: vec![],
                reset_consent_on_fail: false,
            }
        );

        consent
            .command(&Command::DesiredGeneralConsent(
                DesiredGeneralConsentCommand {
                    general_consent: vec!["foo".to_string(), "bar".to_string()],
                },
            ))
            .await
            .unwrap();

        assert_eq!(
            rx_reported_properties.recv().await,
            Some(json!({
            "device_update_consent": {
                "general_consent": ["bar", "foo"],
                "reset_consent_on_fail": false
            }}))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn user_consent_test() {
        let (tx_reported_properties, _rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let mut consent = DeviceUpdateConsent {
            file_observer: None,
            tx_reported_properties: Some(tx_reported_properties),
        };

        assert!(
            consent
                .command(&Command::UserConsent(UserConsentCommand {
                    user_consent: HashMap::from([("foo/bar".to_string(), "bar".to_string())]),
                }))
                .await
                .unwrap_err()
                .chain()
                .any(|e| e
                    .to_string()
                    .starts_with("user_consent: invalid component name: foo/bar"))
        );

        assert!(
            consent
                .command(&Command::UserConsent(UserConsentCommand {
                    user_consent: HashMap::from([("foo".to_string(), "bar".to_string())]),
                }))
                .await
                .unwrap_err()
                .chain()
                .any(|e| e.to_string().starts_with("failed to open for write: "))
        );

        let tmp_dir = tempfile::tempdir().unwrap();
        crate::common::set_env_var("CONSENT_DIR_PATH", tmp_dir.path());
        let foo_dir = tmp_dir.path().join("foo");
        std::fs::create_dir(foo_dir.clone()).unwrap();
        std::fs::copy(
            "testfiles/positive/test_component/user_consent.json",
            foo_dir.join("user_consent.json"),
        )
        .unwrap();

        consent
            .command(&Command::UserConsent(UserConsentCommand {
                user_consent: HashMap::from([("foo".to_string(), "bar".to_string())]),
            }))
            .await
            .unwrap();
    }
}
