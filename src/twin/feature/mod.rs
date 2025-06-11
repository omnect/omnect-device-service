use crate::twin::{
    TwinUpdate, TwinUpdateState, consent, factory_reset, firmware_update, network, reboot,
    ssh_tunnel, system_info,
};
use anyhow::{Context, Result, anyhow, bail};
use azure_iot_sdk::client::{DirectMethod, IotMessage};
use futures::Stream;
use log::{error, info, warn};
use notify_debouncer_full::{DebounceEventResult, Debouncer, NoCache, new_debouncer, notify::*};
use std::hash::{Hash, Hasher};
use std::{
    collections::HashMap,
    hash::DefaultHasher,
    path::PathBuf,
    pin::Pin,
    sync::{LazyLock, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
// ToDo really needed?
use typeid::ConstTypeId;

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    CloseSshTunnel(ssh_tunnel::CloseSshTunnelCommand),
    DesiredGeneralConsent(consent::DesiredGeneralConsentCommand),
    DesiredUpdateDeviceSshCa(ssh_tunnel::UpdateDeviceSshCaCommand),
    FactoryReset(factory_reset::FactoryResetCommand),
    FleetId(system_info::FleetIdCommand),
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
    WatchPath(PathCommand),
}

impl Command {
    pub fn feature_id(&self) -> ConstTypeId {
        use Command::*;

        match self {
            CloseSshTunnel(_) => ConstTypeId::of::<ssh_tunnel::SshTunnel>(),
            DesiredGeneralConsent(_) => ConstTypeId::of::<consent::DeviceUpdateConsent>(),
            DesiredUpdateDeviceSshCa(_) => ConstTypeId::of::<ssh_tunnel::SshTunnel>(),
            FactoryReset(_) => ConstTypeId::of::<factory_reset::FactoryReset>(),
            FleetId(_) => ConstTypeId::of::<system_info::SystemInfo>(),
            GetSshPubKey(_) => ConstTypeId::of::<ssh_tunnel::SshTunnel>(),
            Interval(cmd) => cmd.feature_id,
            LoadFirmwareUpdate(_) => ConstTypeId::of::<firmware_update::FirmwareUpdate>(),
            OpenSshTunnel(_) => ConstTypeId::of::<ssh_tunnel::SshTunnel>(),
            Reboot => ConstTypeId::of::<reboot::Reboot>(),
            ReloadNetwork => ConstTypeId::of::<network::Network>(),
            RunFirmwareUpdate(_) => ConstTypeId::of::<firmware_update::FirmwareUpdate>(),
            SetWaitOnlineTimeout(_) => ConstTypeId::of::<reboot::Reboot>(),
            ValidateUpdate(_) => ConstTypeId::of::<firmware_update::FirmwareUpdate>(),
            UserConsent(_) => ConstTypeId::of::<consent::DeviceUpdateConsent>(),
            WatchPath(cmd) => cmd.feature_id,
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

    async fn command(&mut self, _cmd: &Command) -> CommandResult {
        unimplemented!();
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IntervalCommand {
    pub feature_id: ConstTypeId,
    pub id: Option<u64>,
}

#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub struct PathCommand {
    pub feature_id: ConstTypeId,
    pub path: PathBuf,
}

#[derive(Hash, Eq, PartialEq, Debug)]
pub struct Watch {
    pub command: PathCommand,
    // ToDo: make hashset (https://stackoverflow.com/questions/55932914/how-to-implement-the-stdhashhash-trait-on-external-data-types-in-rust)
    pub event_kinds: Vec<EventKind>,
}
static WATCHER: OnceLock<Mutex<Debouncer<INotifyWatcher, NoCache>>> = OnceLock::new();
static WATCHES: LazyLock<Mutex<HashMap<u64, Watch>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static TIMERS: LazyLock<Mutex<JoinSet<()>>> = LazyLock::new(|| Mutex::new(JoinSet::new()));
static TX_COMMAND_REQUEST: OnceLock<Mutex<mpsc::Sender<CommandRequest>>> = OnceLock::new();

pub fn init(tx_command_request: mpsc::Sender<CommandRequest>) -> Result<()> {
    TX_COMMAND_REQUEST
        .set(Mutex::new(tx_command_request.clone()))
        .map_err(|e| anyhow!("TX_COMMAND_REQUEST: {e:#?}"))?;

    WATCHER
        .set(Mutex::new(
            new_debouncer(
                Duration::from_secs(2),
                None,
                move |res: DebounceEventResult| match res {
                    Ok(debounced_events) => {
                        for de in debounced_events {
                            WATCHES
                                .lock()
                                .unwrap()
                                .values()
                                .filter(|w| {
                                    w.event_kinds.contains(&de.event.kind)
                                        && de.paths.contains(&w.command.path)
                                })
                                .for_each(|w| {
                                    tx_command_request
                                        .blocking_send(CommandRequest {
                                            command: Command::WatchPath(w.command.clone()),
                                            reply: None,
                                        })
                                        .unwrap()
                                });
                        }
                    }
                    Err(errors) => errors.iter().for_each(|e| error!("notify-error: {e:?}")),
                },
            )
            .context("context")?,
        ))
        .map_err(|e| anyhow!("WATCHER: {e:#?}"))
}

pub fn notify_interval(command: IntervalCommand, interval: Duration) {
    TIMERS.lock().unwrap().spawn(async move {
        let mut interval = tokio::time::interval(interval);
        let tx_command_request = TX_COMMAND_REQUEST.get().unwrap().lock().unwrap().clone();
        loop {
            interval.tick().await;
            tx_command_request
                .blocking_send(CommandRequest {
                    command: Command::Interval(command.clone()),
                    reply: None,
                })
                .unwrap()
        }
    });
}

pub fn watch_path(watch: Watch, recursive_mode: RecursiveMode) -> Result<u64> {
    // ToDo filecreated event but file is already there
    // ToDO rm unwrap
    let mut watches = WATCHES.lock().unwrap();
    let mut hasher = DefaultHasher::new();

    watch.hash(&mut hasher);

    let key = hasher.finish();

    if !watches
        .values()
        .any(|w| w.command.path == watch.command.path)
    {
        // ToDO rm unwrap
        WATCHER
            .get()
            .context("context")?
            .lock()
            .unwrap()
            .watch(watch.command.path.clone(), recursive_mode)
            .context("context")?;
    }

    watches.insert(key, watch);

    Ok(key)
}

pub fn unwatch_path(key: u64) -> Result<()> {
    // ToDO rm unwrap
    let mut watches = WATCHES.lock().unwrap();
    if let Some(watch) = watches.remove(&key) {
        if !watches
            .values()
            .any(|w| w.command.path == watch.command.path)
        {
            // ToDO rm unwrap
            WATCHER
                .get()
                .context("context")?
                .lock()
                .unwrap()
                .unwatch(watch.command.path)
                .context("context")?
        }
    }

    Ok(())
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
                user_consent: HashMap::from([("foo".to_string(), "bar".to_string())]),
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
