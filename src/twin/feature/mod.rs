use super::consent::{DesiredGeneralConsentCommand, UserConsentCommand};
use super::factory_reset::FactoryResetCommand;
use super::reboot::SetWaitOnlineTimeoutCommand;
use super::ssh_tunnel::{CloseSshTunnelCommand, GetSshPubKeyCommand, OpenSshTunnelCommand};
use super::{TwinUpdate, TwinUpdateState};
use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::DirectMethod;
use azure_iot_sdk::client::IotMessage;
use futures::Stream;
use futures::StreamExt;
use log::{debug, error, info, warn};
use notify_debouncer_full::{new_debouncer, notify::*, DebounceEventResult, Debouncer, NoCache};
use std::any::TypeId;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;
use tokio::{
    sync::mpsc,
    time::{Instant, Interval},
};

#[derive(Debug, PartialEq)]
pub enum Command {
    CloseSshTunnel(CloseSshTunnelCommand),
    DesiredGeneralConsent(DesiredGeneralConsentCommand),
    FactoryReset(FactoryResetCommand),
    GetSshPubKey(GetSshPubKeyCommand),
    OpenSshTunnel(OpenSshTunnelCommand),
    Reboot,
    ReloadNetwork,
    SetWaitOnlineTimeout(SetWaitOnlineTimeoutCommand),
    UserConsent(UserConsentCommand),
}

impl Command {
    pub fn feature_id(&self) -> TypeId {
        use Command::*;

        match self {
            CloseSshTunnel(_) => TypeId::of::<super::SshTunnel>(),
            DesiredGeneralConsent(_) => TypeId::of::<super::DeviceUpdateConsent>(),
            FactoryReset(_) => TypeId::of::<super::FactoryReset>(),
            GetSshPubKey(_) => TypeId::of::<super::SshTunnel>(),
            OpenSshTunnel(_) => TypeId::of::<super::SshTunnel>(),
            Reboot => TypeId::of::<super::Reboot>(),
            ReloadNetwork => TypeId::of::<super::Network>(),
            SetWaitOnlineTimeout(_) => TypeId::of::<super::Reboot>(),
            UserConsent(_) => TypeId::of::<super::DeviceUpdateConsent>(),
        }
    }

    // we only log errors and don't fail in this function if input cannot be parsed
    pub fn from_direct_method(direct_method: &DirectMethod) -> Option<Command> {
        info!("direct method: {direct_method:?}");

        // ToDo: write macro or fn for match arms
        match direct_method.name.as_str() {
            "factory_reset" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Some(Command::FactoryReset(c)),
                Err(e) => {
                    error!("cannot parse FactoryReset from direct method payload {e}");
                    None
                }
            },
            "user_consent" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Some(Command::UserConsent(UserConsentCommand { user_consent: c })),
                Err(e) => {
                    error!("cannot parse UserConsent from direct method payload {e}");
                    None
                }
            },
            "get_ssh_pub_key" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Some(Command::GetSshPubKey(c)),
                Err(e) => {
                    error!("cannot parse GetSshPubKey from direct method payload {e}");
                    None
                }
            },
            "open_ssh_tunnel" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Some(Command::OpenSshTunnel(c)),
                Err(e) => {
                    error!("cannot parse OpenSshTunnel from direct method payload {e}");
                    None
                }
            },
            "close_ssh_tunnel" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Some(Command::CloseSshTunnel(c)),
                Err(e) => {
                    error!("cannot parse CloseSshTunnel from direct method payload {e}");
                    None
                }
            },
            "reboot" => Some(Command::Reboot),
            "set_wait_online_timeout" => {
                match serde_json::from_value(direct_method.payload.clone()) {
                    Ok(c) => Some(Command::SetWaitOnlineTimeout(c)),
                    Err(e) => {
                        error!("cannot parse CloseSshTunnel from direct method payload {e}");
                        None
                    }
                }
            }
            _ => {
                error!(
                    "cannot parse direct method {} with payload {}",
                    direct_method.name, direct_method.payload
                );
                None
            }
        }
    }

    // we only log errors and don't fail in this function if input cannot be parsed
    pub fn from_desired_property(update: TwinUpdate) -> Vec<Command> {
        info!("desired property: {update:?}");
        let mut cmds = vec![];

        let value = match update.state {
            TwinUpdateState::Partial => &update.value,
            TwinUpdateState::Complete => &update.value["desired"],
        };

        if let Some(map) = value.as_object() {
            for k in map.keys() {
                match k.as_str() {
                    "general_consent" => match serde_json::from_value(value.clone()) {
                        Ok(c) => cmds.push(Command::DesiredGeneralConsent(c)),
                        Err(e) => error!(
                            "from_desired_property: cannot parse DesiredGeneralConsentCommand {e}"
                        ),
                    },
                    "$version" => { /*ignore*/ }
                    _ => warn!("from_desired_property: unhandled desired property {k}"),
                };
            }
        }

        cmds
    }
}

pub type CommandResult = Result<Option<serde_json::Value>>;

#[async_trait(?Send)]
pub(crate) trait Feature {
    fn name(&self) -> String;
    fn version(&self) -> u8;
    fn is_enabled(&self) -> bool;
    fn ensure(&self) -> Result<()> {
        if !self.is_enabled() {
            bail!("feature disabled: {}", self.name());
        }

        Ok(())
    }

    async fn connect_twin(
        &mut self,
        _tx_reported_properties: mpsc::Sender<serde_json::Value>,
        _tx_outgoing_message: mpsc::Sender<IotMessage>,
    ) -> Result<()> {
        Ok(())
    }

    async fn connect_web_service(&self) -> Result<()> {
        Ok(())
    }

    fn event_stream(&mut self) -> Result<Option<EventStream>> {
        Ok(None)
    }
    // ToDo rm and use command()
    async fn handle_event(&mut self, _event: &EventData) -> Result<()> {
        unimplemented!();
    }

    async fn command(&mut self, _cmd: Command) -> CommandResult {
        unimplemented!();
    }
}

#[derive(Debug)]
pub enum EventData {
    FileCreated(PathBuf),
    FileModified(PathBuf),
    Interval(Instant),
}

pub struct Event {
    pub feature_id: TypeId,
    pub data: EventData,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

pub fn interval_stream<T>(interval: Interval) -> EventStream
where
    T: 'static,
{
    tokio_stream::wrappers::IntervalStream::new(interval)
        .map(|i| Event {
            feature_id: TypeId::of::<T>(),
            data: EventData::Interval(i),
        })
        .boxed()
}

pub fn file_created_stream<T>(paths: Vec<&Path>) -> EventStream
where
    T: 'static,
{
    let (tx, rx) = mpsc::channel(2);
    let inner_paths: Vec<PathBuf> = paths.into_iter().map(|p| p.to_path_buf()).collect();

    tokio::task::spawn_blocking(move || loop {
        for p in &inner_paths {
            if matches!(p.try_exists(), Ok(true)) {
                let _ = tx.blocking_send(p.clone());
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|p| Event {
            feature_id: TypeId::of::<T>(),
            data: EventData::FileCreated(p),
        })
        .boxed()
}

pub fn file_modified_stream<T>(
    paths: Vec<&Path>,
) -> Result<(Debouncer<INotifyWatcher, NoCache>, EventStream)>
where
    T: 'static,
{
    let (tx, rx) = mpsc::channel(2);
    let mut debouncer = new_debouncer(
        Duration::from_secs(2),
        None,
        move |res: DebounceEventResult| match res {
            Ok(debounced_events) => {
                for de in debounced_events {
                    if let EventKind::Modify(_) = de.event.kind {
                        debug!("notify-event: {de:?}");
                        for p in &de.paths {
                            let _ = tx.blocking_send(p.clone());
                        }
                    }
                }
            }
            Err(errors) => errors.iter().for_each(|e| error!("notify-error: {e:?}")),
        },
    )?;

    for p in paths {
        ensure!(p.is_file(), "{p:?} is not a regular existing file");
        debug!("watch {p:?}");
        debouncer.watch(p, RecursiveMode::NonRecursive)?;
    }

    Ok((
        debouncer,
        tokio_stream::wrappers::ReceiverStream::new(rx)
            .map(|p| Event {
                feature_id: TypeId::of::<T>(),
                data: EventData::FileModified(p),
            })
            .boxed(),
    ))
}

#[cfg(test)]
mod tests {
    use crate::twin::factory_reset;

    use super::*;
    use serde_json::json;
    use tokio::sync::oneshot;

    #[test]
    fn from_direct_method_test() {
        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(Command::from_direct_method(&DirectMethod {
            name: "unknown".to_string(),
            payload: json!({}),
            responder,
        })
        .is_none());

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(Command::from_direct_method(&DirectMethod {
            name: "factory_reset".to_string(),
            payload: json!({}),
            responder,
        })
        .is_none());

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(Command::from_direct_method(&DirectMethod {
            name: "factory_reset".to_string(),
            payload: json!({
                "mode": 0,
                "preserve": ["1"],
            }),
            responder,
        })
        .is_none());

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "factory_reset".to_string(),
                payload: json!({
                    "mode": 1,
                    "preserve": ["1"],
                }),
                responder,
            }),
            Some(Command::FactoryReset(FactoryResetCommand {
                mode: factory_reset::FactoryResetMode::Mode1,
                preserve: vec!["1".to_string()]
            }))
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "factory_reset".to_string(),
                payload: json!({
                    "mode": 1,
                    "preserve": [],
                }),
                responder,
            }),
            Some(Command::FactoryReset(FactoryResetCommand {
                mode: factory_reset::FactoryResetMode::Mode1,
                preserve: vec![]
            }))
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(Command::from_direct_method(&DirectMethod {
            name: "user_consent".to_string(),
            payload: json!({"foo": 1}),
            responder,
        })
        .is_none());

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "user_consent".to_string(),
                payload: json!({"foo": "bar"}),
                responder,
            }),
            Some(Command::UserConsent(UserConsentCommand {
                user_consent: std::collections::HashMap::from([(
                    "foo".to_string(),
                    "bar".to_string()
                )]),
            }))
        );
    }

    #[test]
    fn from_desired_property_test() {
        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Partial,
                value: json!({})
            }),
            vec![]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Partial,
                value: json!({
                    "$version": 1,
                    "general_consent": ["swupdate"]})
            }),
            vec![Command::DesiredGeneralConsent(
                DesiredGeneralConsentCommand {
                    general_consent: vec!["swupdate".to_string()]
                }
            )]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Partial,
                value: json!({"general_consent": []})
            }),
            vec![Command::DesiredGeneralConsent(
                DesiredGeneralConsentCommand {
                    general_consent: vec![]
                }
            )]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Partial,
                value: json!({"general_consent": ["one", "two"]})
            }),
            vec![Command::DesiredGeneralConsent(
                DesiredGeneralConsentCommand {
                    general_consent: vec!["one".to_string(), "two".to_string()]
                }
            )]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Complete,
                value: json!({})
            }),
            vec![]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Complete,
                value: json!({"desired": {}})
            }),
            vec![]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Complete,
                value: json!({"desired": {"general_consent": []}})
            }),
            vec![Command::DesiredGeneralConsent(
                DesiredGeneralConsentCommand {
                    general_consent: vec![]
                }
            )]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Complete,
                value: json!({"desired": {"general_consent": ["one", "two"]}})
            }),
            vec![Command::DesiredGeneralConsent(
                DesiredGeneralConsentCommand {
                    general_consent: vec!["one".to_string(), "two".to_string()]
                }
            )]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Complete,
                value: json!({"desired": {"key": "value"}})
            }),
            vec![]
        );

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Complete,
                value: json!({"desired": {"general_consent": ""}})
            }),
            vec![]
        );
    }
}
