use crate::twin::{
    TwinUpdate, TwinUpdateState, consent, factory_reset, firmware_update, network, reboot,
    ssh_tunnel, system_info,
};
use anyhow::{Context, Result, bail};
use azure_iot_sdk::client::DirectMethod;
use futures::Stream;
use futures::StreamExt;
#[cfg(not(feature = "mock"))]
use log::debug;
use log::{error, info, warn};
use std::{
    any::TypeId,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};
#[cfg(not(feature = "mock"))]
use std::{collections::HashMap, ffi::c_int};
use tokio::{
    sync::{mpsc, oneshot},
    time::Interval,
};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    CloseSshTunnel(ssh_tunnel::CloseSshTunnelCommand),
    DesiredGeneralConsent(consent::DesiredGeneralConsentCommand),
    DesiredUpdateDeviceSshCa(ssh_tunnel::UpdateDeviceSshCaCommand),
    FactoryReset(factory_reset::FactoryResetCommand),
    FleetId(system_info::FleetIdCommand),
    FsEvent(FsEventCommand),
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

fn parse_payload<T: serde::de::DeserializeOwned>(
    payload: &serde_json::Value,
    command_name: &str,
) -> Result<T> {
    serde_json::from_value(payload.clone())
        .with_context(|| format!("cannot parse {command_name} from payload"))
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
            FsEvent(cmd) => cmd.feature_id,
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

    pub fn triggers_reboot(&self) -> bool {
        matches!(
            self,
            Command::FactoryReset(_) | Command::Reboot | Command::RunFirmwareUpdate(_)
        )
    }

    pub fn from_direct_method(direct_method: &DirectMethod) -> Result<Command> {
        info!("direct method: {direct_method:?}");

        let payload = &direct_method.payload;

        match direct_method.name.as_str() {
            "factory_reset" => Ok(Command::FactoryReset(parse_payload(
                payload,
                "factory_reset",
            )?)),
            "user_consent" => Ok(Command::UserConsent(consent::UserConsentCommand {
                user_consent: parse_payload(payload, "user_consent")?,
            })),
            "get_ssh_pub_key" => Ok(Command::GetSshPubKey(parse_payload(
                payload,
                "get_ssh_pub_key",
            )?)),
            "open_ssh_tunnel" => Ok(Command::OpenSshTunnel(parse_payload(
                payload,
                "open_ssh_tunnel",
            )?)),
            "close_ssh_tunnel" => Ok(Command::CloseSshTunnel(parse_payload(
                payload,
                "close_ssh_tunnel",
            )?)),
            "reboot" => Ok(Command::Reboot),
            "set_wait_online_timeout" => Ok(Command::SetWaitOnlineTimeout(parse_payload(
                payload,
                "set_wait_online_timeout",
            )?)),
            _ => bail!(
                "cannot parse direct method {} with payload {}",
                direct_method.name,
                direct_method.payload
            ),
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
                    "ssh_tunnel_ca_pub" => match parse_payload(value, "DesiredUpdateDeviceSshCa") {
                        Ok(c) => cmds.push(Command::DesiredUpdateDeviceSshCa(c)),
                        Err(e) => error!("from_desired_property: {e:#}"),
                    },
                    "general_consent" => {
                        match parse_payload(value, "DesiredGeneralConsentCommand") {
                            Ok(c) => cmds.push(Command::DesiredGeneralConsent(c)),
                            Err(e) => error!("from_desired_property: {e:#}"),
                        }
                    }
                    "fleet_id" => match parse_payload(value, "FleetIdCommand") {
                        Ok(c) => cmds.push(Command::FleetId(c)),
                        Err(e) => error!("from_desired_property: {e:#}"),
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

#[derive(Clone, Debug, PartialEq)]
pub enum FsEventKind {
    DirModified,
    FileCreated,
    FileModified,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FsEventCommand {
    pub kind: FsEventKind,
    pub feature_id: TypeId,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IntervalCommand {
    pub feature_id: TypeId,
}

pub fn interval_stream<T>(interval: Interval) -> CommandRequestStream
where
    T: 'static,
{
    tokio_stream::wrappers::IntervalStream::new(interval)
        .map(|_| CommandRequest {
            command: Command::Interval(IntervalCommand {
                feature_id: TypeId::of::<T>(),
            }),
            reply: None,
        })
        .boxed()
}

const DEBOUNCE_DURATION: Duration = Duration::from_secs(2);

struct WatchInfo {
    feature_id: TypeId,
    path: PathBuf,
    kind: FsEventKind,
    oneshot: bool,
}

#[cfg(not(feature = "mock"))]
pub struct FsWatcher {
    inotify: Option<inotify::Inotify>,
    watches: inotify::Watches,
    watch_info: HashMap<c_int, WatchInfo>,
}

#[cfg(feature = "mock")]
pub struct FsWatcher;

#[cfg(not(feature = "mock"))]
impl FsWatcher {
    pub fn new() -> Result<Self> {
        let inotify = inotify::Inotify::init().context("FsWatcher: failed to init inotify")?;
        let watches = inotify.watches();
        Ok(Self {
            inotify: Some(inotify),
            watches,
            watch_info: HashMap::new(),
        })
    }

    pub fn add_watch<T: 'static>(
        &mut self,
        path: &Path,
        kind: FsEventKind,
        oneshot: bool,
    ) -> Result<()> {
        use inotify::WatchMask;

        let (watch_path, mask) = match kind {
            FsEventKind::FileCreated => {
                let parent = path
                    .parent()
                    .context("add_watch: FileCreated path has no parent")?;
                let mut mask = WatchMask::CREATE;
                if oneshot {
                    mask |= WatchMask::ONESHOT;
                }
                (parent, mask)
            }
            FsEventKind::FileModified => (path, WatchMask::CLOSE_WRITE),
            FsEventKind::DirModified => (path, WatchMask::CREATE | WatchMask::DELETE),
        };

        let wd = self
            .watches
            .add(watch_path, mask)
            .with_context(|| format!("add_watch: failed to watch {watch_path:?}"))?;

        debug!(
            "add_watch: {:?} on {watch_path:?} (target: {path:?}, oneshot: {oneshot})",
            wd.get_watch_descriptor_id()
        );

        self.watch_info.insert(
            wd.get_watch_descriptor_id(),
            WatchInfo {
                feature_id: TypeId::of::<T>(),
                path: path.to_path_buf(),
                kind,
                oneshot,
            },
        );

        Ok(())
    }

    pub fn into_stream(mut self, tx: mpsc::Sender<CommandRequest>) -> Result<()> {
        let inotify = self
            .inotify
            .take()
            .context("into_stream: inotify already consumed")?;

        // Race-condition handling for FileCreated watches: if file already exists,
        // send the event immediately. Watch was set up on parent dir first, so any
        // creation between watch-setup and this check will also fire an inotify event
        // (duplicate is harmless).
        for info in self.watch_info.values() {
            if info.kind == FsEventKind::FileCreated && matches!(info.path.try_exists(), Ok(true)) {
                debug!(
                    "into_stream: file already exists {:?}, sending immediately",
                    info.path
                );
                let _ = tx.blocking_send(CommandRequest {
                    command: Command::FsEvent(FsEventCommand {
                        kind: FsEventKind::FileCreated,
                        feature_id: info.feature_id,
                        path: info.path.clone(),
                    }),
                    reply: None,
                });
            }
        }

        tokio::spawn(async move {
            let mut buffer = [0; 1024];
            let mut stream = inotify
                .into_event_stream(&mut buffer)
                .expect("into_stream: failed to create event stream");

            // Per-watch debounce: maps wd_id -> deadline. When deadline expires
            // without new events, the command is dispatched.
            let mut debounce_deadlines: HashMap<c_int, tokio::time::Instant> = HashMap::new();

            loop {
                let next_deadline = debounce_deadlines
                    .values()
                    .min()
                    .copied()
                    .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));

                tokio::select! {
                    biased;

                    event = stream.next() => {
                        let Some(event) = event else {
                            warn!("FsWatcher: inotify stream ended");
                            return;
                        };
                        let event = match event {
                            Ok(e) => e,
                            Err(e) => {
                                error!("FsWatcher: inotify error: {e:#}");
                                continue;
                            }
                        };

                        let wd_id = event.wd.get_watch_descriptor_id();
                        let Some(info) = self.watch_info.get(&wd_id) else {
                            debug!("FsWatcher: unknown wd {wd_id}");
                            continue;
                        };

                        // For FileCreated: filter by filename match
                        if info.kind == FsEventKind::FileCreated {
                            let expected_name = info.path.file_name();
                            let event_name = event.name.as_deref();
                            if expected_name.is_none() || event_name != expected_name {
                                continue;
                            }
                        }

                        debug!("FsWatcher: event for {:?} ({:?})", info.path, info.kind);

                        if info.oneshot {
                            let _ = tx.send(CommandRequest {
                                command: Command::FsEvent(FsEventCommand {
                                    kind: info.kind.clone(),
                                    feature_id: info.feature_id,
                                    path: info.path.clone(),
                                }),
                                reply: None,
                            }).await;
                            self.watch_info.remove(&wd_id);
                        } else {
                            debounce_deadlines.insert(
                                wd_id,
                                tokio::time::Instant::now() + DEBOUNCE_DURATION,
                            );
                        }
                    }

                    _ = tokio::time::sleep_until(next_deadline), if !debounce_deadlines.is_empty() => {
                        let now = tokio::time::Instant::now();
                        let expired: Vec<c_int> = debounce_deadlines
                            .iter()
                            .filter(|(_, deadline)| **deadline <= now)
                            .map(|(wd_id, _)| *wd_id)
                            .collect();

                        for wd_id in expired {
                            debounce_deadlines.remove(&wd_id);
                            if let Some(info) = self.watch_info.get(&wd_id) {
                                debug!("FsWatcher: debounce elapsed for {:?}", info.path);
                                let _ = tx.send(CommandRequest {
                                    command: Command::FsEvent(FsEventCommand {
                                        kind: info.kind.clone(),
                                        feature_id: info.feature_id,
                                        path: info.path.clone(),
                                    }),
                                    reply: None,
                                }).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

#[cfg(feature = "mock")]
impl FsWatcher {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }

    pub fn add_watch<T: 'static>(
        &mut self,
        _path: &Path,
        _kind: FsEventKind,
        _oneshot: bool,
    ) -> Result<()> {
        Ok(())
    }

    pub fn into_stream(self, _tx: mpsc::Sender<CommandRequest>) -> Result<()> {
        Ok(())
    }
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
    fn triggers_reboot_test() {
        assert!(Command::Reboot.triggers_reboot());
        assert!(
            Command::FactoryReset(factory_reset::FactoryResetCommand {
                mode: factory_reset::FactoryResetMode::Mode1,
                preserve: vec![]
            })
            .triggers_reboot()
        );
        assert!(
            Command::RunFirmwareUpdate(firmware_update::RunUpdateCommand {
                validate_iothub_connection: false
            })
            .triggers_reboot()
        );

        // Test some commands that should not trigger reboot
        assert!(!Command::ReloadNetwork.triggers_reboot());
        assert!(!Command::ValidateUpdate(true).triggers_reboot());
    }

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
