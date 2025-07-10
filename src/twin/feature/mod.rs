use crate::twin::{
    TwinUpdate, TwinUpdateState, consent, factory_reset, firmware_update, network, reboot,
    ssh_tunnel, system_info,
};
use anyhow::{Context, Result, anyhow, bail};
use azure_iot_sdk::client::{DirectMethod, IotMessage};
use futures::Stream;
use futures_util::StreamExt;
use inotify::{Event, Inotify, WatchDescriptor, WatchMask, Watches};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::ffi::OsString;
use std::{
    any::TypeId,
    path::Path,
    pin::Pin,
    sync::{LazyLock, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};

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
    WatchPath(WatchPathCommand),
}

impl Command {
    pub fn feature_id(&self) -> TypeId {
        use Command::*;

        match self {
            CloseSshTunnel(_) => TypeId::of::<ssh_tunnel::SshTunnel>(),
            DesiredGeneralConsent(_) => TypeId::of::<consent::DeviceUpdateConsent>(),
            DesiredUpdateDeviceSshCa(_) => TypeId::of::<ssh_tunnel::SshTunnel>(),
            FactoryReset(_) => TypeId::of::<factory_reset::FactoryReset>(),
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
    pub feature_id: TypeId,
}

#[derive(Clone, Debug)]
pub struct WatchPathCommand {
    pub feature_id: TypeId,
    pub event: Event<OsString>,
}

#[derive(Debug)]
struct FileWatcher {
    watches: Watches,
    feature_map: HashMap<WatchDescriptor, TypeId>,
}

impl PartialEq for WatchPathCommand {
    fn eq(&self, other: &Self) -> bool {
        self.event.wd == other.event.wd
    }
}

impl Eq for WatchPathCommand {}

static TASKS: LazyLock<Mutex<JoinSet<()>>> = LazyLock::new(|| Mutex::new(JoinSet::new()));
static FILE_WATCHER: OnceLock<Mutex<FileWatcher>> = OnceLock::new();
static TX_COMMAND_REQUEST: OnceLock<mpsc::Sender<CommandRequest>> = OnceLock::new();

pub fn init(tx_command_request: mpsc::Sender<CommandRequest>) -> Result<()> {
    TX_COMMAND_REQUEST
        .set(tx_command_request.clone())
        .map_err(|e| anyhow!("init: set TX_COMMAND_REQUEST {e:#?}"))?;

    let inotify = Inotify::init().context("init: failed to initialize inotify")?;

    FILE_WATCHER
        .set(Mutex::new(FileWatcher {
            watches: inotify.watches(),
            feature_map: HashMap::new(),
        }))
        .map_err(|e| anyhow!("init: set FILE_WATCHER {e:#?}"))?;

    TASKS
        .lock()
        .map_err(|e| anyhow!("init: TASKS {e:#?}"))?
        .spawn(async move {
            let mut buffer = [0; 1024];
            let mut stream = inotify.into_event_stream(&mut buffer).unwrap();
            while let Some(event_result) = stream.next().await {
                match event_result {
                    Ok(ev) => {
                        debug!("inotify: {ev:?}");
                        let feature_id = *FILE_WATCHER
                            .get()
                            .unwrap()
                            .lock()
                            .unwrap()
                            .feature_map
                            .get(&ev.wd)
                            .unwrap();

                        if let Err(e) = tx_command_request
                            .send(CommandRequest {
                                command: Command::WatchPath(WatchPathCommand {
                                    feature_id,
                                    event: ev,
                                }),
                                reply: None,
                            })
                            .await
                        {
                            error!("inotify: send {e:#?}")
                        }
                    }
                    Err(e) => error!("inotify: event {e:#?}"),
                }
            }
        });

    Ok(())
}

pub fn add_watch<T>(path: &Path, mask: WatchMask) -> Result<WatchDescriptor>
where
    T: 'static,
{
    let mut watcher = FILE_WATCHER
        .get()
        .context("add_watch: FileWatcher missing")?
        .lock()
        .map_err(|e| anyhow!("add_watch: FILE_WATCHER {e:#?}"))?;

    let desc = watcher.watches.add(path, mask)?;

    let None = watcher.feature_map.insert(desc.clone(), TypeId::of::<T>()) else {
        bail!("add_watch: currently only one feature per watch supported")
    };

    Ok(desc)
}

pub fn remove_watch(wd: WatchDescriptor) -> Result<()> {
    let mut watcher = FILE_WATCHER
        .get()
        .context("remove_watch: FileWatcher missing")?
        .lock()
        .map_err(|e| anyhow!("remove_watch: FILE_WATCHER {e:#?}"))?;

    watcher.watches.remove(wd.clone())?;

    if watcher.feature_map.remove_entry(&wd).is_none() {
        warn!("remove_watch: WatchDescriptor doesn't exist")
    };

    Ok(())
}

pub fn notify_interval<T>(interval: Duration) -> Result<()>
where
    T: 'static,
{
    let tx_command_request = TX_COMMAND_REQUEST
        .get()
        .context("notify_interval: failed to get TX_COMMAND_REQUEST")?
        .clone();

    TASKS
        .lock()
        .map_err(|e| anyhow!("notify_interval: TASKS {e:#?}"))?
        .spawn(async move {
            let mut interval = tokio::time::interval(interval);
            loop {
                interval.tick().await;
                if let Err(e) = tx_command_request
                    .send(CommandRequest {
                        command: Command::Interval(IntervalCommand {
                            feature_id: TypeId::of::<T>(),
                        }),
                        reply: None,
                    })
                    .await
                {
                    error!("notify_interval: send {e:#?}")
                }
            }
        });

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
