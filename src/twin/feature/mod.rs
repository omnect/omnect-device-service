use crate::twin::{
    TwinUpdate, TwinUpdateState, consent, factory_reset, firmware_update, network, reboot,
    ssh_tunnel, system_info,
};
use anyhow::{Result, bail, ensure};
use azure_iot_sdk::client::{DirectMethod, IotMessage};
use futures::Stream;
use futures::StreamExt;
use log::{debug, error, info, warn};
use notify_debouncer_full::{DebounceEventResult, Debouncer, NoCache, new_debouncer, notify::*};
use std::{
    any::TypeId,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, Interval},
};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    CloseSshTunnel(ssh_tunnel::CloseSshTunnelCommand),
    DesiredGeneralConsent(consent::DesiredGeneralConsentCommand),
    DesiredUpdateDeviceSshCa(ssh_tunnel::UpdateDeviceSshCaCommand),
    DirModified(PathCommand),
    FactoryReset(factory_reset::FactoryResetCommand),
    FleetId(system_info::FleetIdCommand),
    FileCreated(PathCommand),
    FileModified(PathCommand),
    GetSshPubKey(ssh_tunnel::GetSshPubKeyCommand),
    Interval(IntervalCommand),
    LoadFirmwareUpdate(firmware_update::LoadUpdateCommand),
    OpenSshTunnel(ssh_tunnel::OpenSshTunnelCommand),
    Reboot,
    ReloadNetwork,
    RunFirmwareUpdate(firmware_update::RunUpdateCommand),
    SetWaitOnlineTimeout(reboot::SetWaitOnlineTimeoutCommand),
    ValidateUpdate(bool),
    UserConsent(consent::UserConsentCommand),
}

impl Command {
    pub fn feature_id(&self) -> TypeId {
        use Command::*;

        match self {
            CloseSshTunnel(_) => TypeId::of::<ssh_tunnel::SshTunnel>(),
            DesiredGeneralConsent(_) => TypeId::of::<consent::DeviceUpdateConsent>(),
            DesiredUpdateDeviceSshCa(_) => TypeId::of::<ssh_tunnel::SshTunnel>(),
            DirModified(cmd) => cmd.feature_id,
            FactoryReset(_) => TypeId::of::<factory_reset::FactoryReset>(),
            FileCreated(cmd) => cmd.feature_id,
            FileModified(cmd) => cmd.feature_id,
            FleetId(_) => TypeId::of::<system_info::SystemInfo>(),
            GetSshPubKey(_) => TypeId::of::<ssh_tunnel::SshTunnel>(),
            Interval(cmd) => cmd.feature_id,
            LoadFirmwareUpdate(_) => TypeId::of::<firmware_update::FirmwareUpdate>(),
            OpenSshTunnel(_) => TypeId::of::<ssh_tunnel::SshTunnel>(),
            Reboot => TypeId::of::<reboot::Reboot>(),
            ReloadNetwork => TypeId::of::<network::Network>(),
            RunFirmwareUpdate(_) => TypeId::of::<firmware_update::FirmwareUpdate>(),
            SetWaitOnlineTimeout(_) => TypeId::of::<reboot::Reboot>(),
            ValidateUpdate(_) => TypeId::of::<firmware_update::FirmwareUpdate>(),
            UserConsent(_) => TypeId::of::<consent::DeviceUpdateConsent>(),
        }
    }

    pub fn from_direct_method(direct_method: &DirectMethod) -> Result<Command> {
        info!("direct method: {direct_method:?}");

        // ToDo: write macro or fn for match arms
        match direct_method.name.as_str() {
            "factory_reset" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Ok(Command::FactoryReset(c)),
                Err(e) => {
                    bail!("cannot parse FactoryReset from direct method payload {e}")
                }
            },
            "user_consent" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Ok(Command::UserConsent(consent::UserConsentCommand {
                    user_consent: c,
                })),
                Err(e) => {
                    bail!("cannot parse UserConsent from direct method payload {e}")
                }
            },
            "get_ssh_pub_key" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Ok(Command::GetSshPubKey(c)),
                Err(e) => {
                    bail!("cannot parse GetSshPubKey from direct method payload {e}")
                }
            },
            "open_ssh_tunnel" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Ok(Command::OpenSshTunnel(c)),
                Err(e) => {
                    bail!("cannot parse OpenSshTunnel from direct method payload {e}")
                }
            },
            "close_ssh_tunnel" => match serde_json::from_value(direct_method.payload.clone()) {
                Ok(c) => Ok(Command::CloseSshTunnel(c)),
                Err(e) => {
                    bail!("cannot parse CloseSshTunnel from direct method payload {e}")
                }
            },
            "reboot" => Ok(Command::Reboot),
            "set_wait_online_timeout" => {
                match serde_json::from_value(direct_method.payload.clone()) {
                    Ok(c) => Ok(Command::SetWaitOnlineTimeout(c)),
                    Err(e) => {
                        bail!("cannot parse CloseSshTunnel from direct method payload {e}")
                    }
                }
            }
            _ => {
                bail!(
                    "cannot parse direct method {} with payload {}",
                    direct_method.name,
                    direct_method.payload
                )
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
                    "ssh_tunnel_ca_pub" => match serde_json::from_value(value.clone()) {
                        Ok(c) => cmds.push(Command::DesiredUpdateDeviceSshCa(c)),
                        Err(e) => error!(
                            "from_desired_property: cannot parse DesiredUpdateDeviceSshCa {e:#}"
                        ),
                    },
                    "general_consent" => match serde_json::from_value(value.clone()) {
                        Ok(c) => cmds.push(Command::DesiredGeneralConsent(c)),
                        Err(e) => error!(
                            "from_desired_property: cannot parse DesiredGeneralConsentCommand {e:#}"
                        ),
                    },
                    "fleet_id" => match serde_json::from_value(value.clone()) {
                        Ok(c) => cmds.push(Command::FleetId(c)),
                        Err(e) => {
                            error!("from_desired_property: cannot parse FleetIdCommand {e:#}")
                        }
                    },
                    "$version" => { /*ignore*/ }
                    _ => warn!("from_desired_property: unhandled desired property {k}"),
                };
            }
        }

        cmds
    }
}

#[derive(Debug)]
pub struct CommandRequest {
    pub command: Command,
    pub reply: Option<oneshot::Sender<CommandResult>>,
}

pub type CommandResult = Result<Option<serde_json::Value>>;
pub type CommandRequestStream = Pin<Box<dyn Stream<Item = CommandRequest> + Send>>;
pub type CommandRequestStreamResult = Result<Option<CommandRequestStream>>;

#[dynosaur::dynosaur(pub DynFeature)]
pub(crate) trait Feature {
    fn name(&self) -> String;
    fn version(&self) -> u8;
    fn is_enabled(&self) -> bool;

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

    fn command_request_stream(&mut self) -> CommandRequestStreamResult {
        Ok(None)
    }

    async fn command(&mut self, _cmd: &Command) -> CommandResult {
        unimplemented!();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PathCommand {
    pub feature_id: TypeId,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IntervalCommand {
    pub feature_id: TypeId,
    pub instant: Instant,
}

pub fn interval_stream<T>(interval: Interval) -> CommandRequestStream
where
    T: 'static,
{
    tokio_stream::wrappers::IntervalStream::new(interval)
        .map(|i| CommandRequest {
            command: Command::Interval(IntervalCommand {
                feature_id: TypeId::of::<T>(),
                instant: i,
            }),
            reply: None,
        })
        .boxed()
}

pub fn file_created_stream<T>(paths: Vec<&Path>) -> Result<(JoinHandle<()>, CommandRequestStream)>
where
    T: 'static,
{
    let (tx, rx) = mpsc::channel(2);
    let inner_paths: Vec<PathBuf> = paths.into_iter().map(|p| p.to_path_buf()).collect();

    let handle = tokio::task::spawn_blocking(move || {
        loop {
            for p in &inner_paths {
                if matches!(p.try_exists(), Ok(true)) {
                    let _ = tx.blocking_send(CommandRequest {
                        command: Command::FileCreated(PathCommand {
                            feature_id: TypeId::of::<T>(),
                            path: p.clone(),
                        }),
                        reply: None,
                    });
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });

    Ok((
        handle,
        tokio_stream::wrappers::ReceiverStream::new(rx).boxed(),
    ))
}

pub fn file_modified_stream<T>(
    paths: Vec<&Path>,
) -> Result<(Debouncer<INotifyWatcher, NoCache>, CommandRequestStream)>
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
                            let _ = tx.blocking_send(CommandRequest {
                                command: Command::FileModified(PathCommand {
                                    feature_id: TypeId::of::<T>(),
                                    path: p.clone(),
                                }),
                                reply: None,
                            });
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
        tokio_stream::wrappers::ReceiverStream::new(rx).boxed(),
    ))
}

pub fn dir_modified_stream<T>(
    paths: Vec<&Path>,
) -> Result<(Debouncer<INotifyWatcher, NoCache>, CommandRequestStream)>
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
                    if matches!(de.event.kind, EventKind::Create(_) | EventKind::Remove(_)) {
                        debug!("notify-event: {de:?}");
                        for p in &de.paths {
                            let _ = tx.blocking_send(CommandRequest {
                                command: Command::DirModified(PathCommand {
                                    feature_id: TypeId::of::<T>(),
                                    path: p.clone(),
                                }),
                                reply: None,
                            });
                        }
                    }
                }
            }
            Err(errors) => errors.iter().for_each(|e| error!("notify-error: {e:?}")),
        },
    )?;

    for p in paths {
        ensure!(p.is_dir(), "{p:?} is not a regular existing directory");
        debug!("watch {p:?}");
        debouncer.watch(p, RecursiveMode::Recursive)?;
    }

    Ok((
        debouncer,
        tokio_stream::wrappers::ReceiverStream::new(rx).boxed(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::twin::factory_reset;
    use reboot::SetWaitOnlineTimeoutCommand;
    use serde_json::json;
    use std::str::FromStr;
    use tokio::sync::oneshot;

    #[test]
    fn from_direct_method_test() {
        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(
            Command::from_direct_method(&DirectMethod {
                name: "unknown".to_string(),
                payload: json!({}),
                responder,
            })
            .is_err()
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(
            Command::from_direct_method(&DirectMethod {
                name: "factory_reset".to_string(),
                payload: json!({}),
                responder,
            })
            .is_err()
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(
            Command::from_direct_method(&DirectMethod {
                name: "factory_reset".to_string(),
                payload: json!({
                    "mode": 0,
                    "preserve": ["1"],
                }),
                responder,
            })
            .is_err()
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "factory_reset".to_string(),
                payload: json!({
                    "mode": 1,
                    "preserve": ["1"],
                }),
                responder,
            })
            .unwrap(),
            Command::FactoryReset(factory_reset::FactoryResetCommand {
                mode: factory_reset::FactoryResetMode::Mode1,
                preserve: vec!["1".to_string()]
            })
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
            })
            .unwrap(),
            Command::FactoryReset(factory_reset::FactoryResetCommand {
                mode: factory_reset::FactoryResetMode::Mode1,
                preserve: vec![]
            })
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(
            Command::from_direct_method(&DirectMethod {
                name: "user_consent".to_string(),
                payload: json!({"foo": 1}),
                responder,
            })
            .is_err()
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "user_consent".to_string(),
                payload: json!({"foo": "bar"}),
                responder,
            })
            .unwrap(),
            Command::UserConsent(consent::UserConsentCommand {
                user_consent: std::collections::HashMap::from([(
                    "foo".to_string(),
                    "bar".to_string()
                )]),
            })
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(
            Command::from_direct_method(&DirectMethod {
                name: "close_ssh_tunnel".to_string(),
                payload: json!({"tunnel_id": "no-uuid"}),
                responder,
            })
            .is_err()
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "close_ssh_tunnel".to_string(),
                payload: json!({"tunnel_id": "3015d09d-b5e5-4c47-91d1-72460fd67b5d"}),
                responder,
            })
            .unwrap(),
            Command::CloseSshTunnel(ssh_tunnel::CloseSshTunnelCommand {
                tunnel_id: "3015d09d-b5e5-4c47-91d1-72460fd67b5d".to_string(),
            })
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(
            Command::from_direct_method(&DirectMethod {
                name: "get_ssh_pub_key".to_string(),
                payload: json!({"tunnel_id": "no-uuid"}),
                responder,
            })
            .is_err()
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "get_ssh_pub_key".to_string(),
                payload: json!({"tunnel_id": "3015d09d-b5e5-4c47-91d1-72460fd67b5d"}),
                responder,
            })
            .unwrap(),
            Command::GetSshPubKey(ssh_tunnel::GetSshPubKeyCommand {
                tunnel_id: "3015d09d-b5e5-4c47-91d1-72460fd67b5d".to_string(),
            })
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "open_ssh_tunnel".to_string(),
                payload: json!({
                    "tunnel_id": "3015d09d-b5e5-4c47-91d1-72460fd67b5d",
                    "certificate": "cert",
                    "host": "my-host",
                    "port": 22,
                    "user": "usr",
                    "socket_path": "/socket",
                }),
                responder,
            })
            .unwrap(),
            Command::OpenSshTunnel(ssh_tunnel::OpenSshTunnelCommand {
                tunnel_id: "3015d09d-b5e5-4c47-91d1-72460fd67b5d".to_string(),
                certificate: "cert".to_string(),
                bastion_config: ssh_tunnel::BastionConfig {
                    host: "my-host".to_string(),
                    port: 22,
                    user: "usr".to_string(),
                    socket_path: PathBuf::from_str("/socket").unwrap(),
                }
            })
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "get_ssh_pub_key".to_string(),
                payload: json!({"tunnel_id": "3015d09d-b5e5-4c47-91d1-72460fd67b5d"}),
                responder,
            })
            .unwrap(),
            Command::GetSshPubKey(ssh_tunnel::GetSshPubKeyCommand {
                tunnel_id: "3015d09d-b5e5-4c47-91d1-72460fd67b5d".to_string(),
            })
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "reboot".to_string(),
                payload: json!({}),
                responder,
            })
            .unwrap(),
            Command::Reboot
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "set_wait_online_timeout".to_string(),
                payload: json!({}),
                responder,
            })
            .unwrap(),
            Command::SetWaitOnlineTimeout(SetWaitOnlineTimeoutCommand { timeout_secs: None })
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert_eq!(
            Command::from_direct_method(&DirectMethod {
                name: "set_wait_online_timeout".to_string(),
                payload: json!({"timeout_secs": 1}),
                responder,
            })
            .unwrap(),
            Command::SetWaitOnlineTimeout(SetWaitOnlineTimeoutCommand {
                timeout_secs: Some(1),
            })
        );

        let (responder, _rx) = oneshot::channel::<CommandResult>();
        assert!(
            Command::from_direct_method(&DirectMethod {
                name: "set_wait_online_timeout".to_string(),
                payload: json!({"timeout_secs": "1"}),
                responder,
            })
            .is_err()
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
                consent::DesiredGeneralConsentCommand {
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
                consent::DesiredGeneralConsentCommand {
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
                consent::DesiredGeneralConsentCommand {
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
                consent::DesiredGeneralConsentCommand {
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
                consent::DesiredGeneralConsentCommand {
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

        assert_eq!(
            Command::from_desired_property(TwinUpdate {
                state: TwinUpdateState::Complete,
                value: json!({"desired": {"fleet_id": ""}})
            }),
            vec![Command::FleetId(system_info::FleetIdCommand {
                fleet_id: "".to_string()
            })]
        );
    }
}
