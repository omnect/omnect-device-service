use crate::twin::{
    TwinUpdate, TwinUpdateState, consent, factory_reset, firmware_update, network, reboot,
    ssh_tunnel, system_info,
};
use anyhow::{Context, Result, bail};
use azure_iot_sdk::client::DirectMethod;
use futures::{Stream, StreamExt, stream};
use log::{debug, error, info, warn};
use std::{any::TypeId, future::Future, path::PathBuf, pin::Pin, time::Duration};

pub(crate) const FAR_FUTURE: Duration = Duration::from_secs(3600);
use tokio::sync::{mpsc, oneshot};
use tokio::time::Interval;
use tokio_util::sync::CancellationToken;

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

#[derive(Clone, Copy, Debug, PartialEq)]
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

/// Debounce an async event source into a `CommandRequestStream`.
///
/// `source_fut` is resolved inside the spawned task, so this works from sync
/// call sites (e.g. `command_request_stream()`). After the future resolves,
/// each item from the resulting stream resets a silence timer. When `silence`
/// elapses without new items, a single `Command::Interval` is emitted.
///
/// The spawned task exits when `cancel` is triggered or the source stream ends.
pub fn debounced_command_stream<F, S, I, T>(
    source_fut: F,
    silence: Duration,
    cancel: CancellationToken,
) -> CommandRequestStream
where
    F: Future<Output = Result<S>> + Send + 'static,
    S: Stream<Item = I> + Send + 'static,
    I: Send,
    T: 'static,
{
    // Capacity 1: debounce guarantees at most one pending request
    // (next emission only after the previous silence window expires).
    let (tx, rx) = tokio::sync::mpsc::channel::<CommandRequest>(1);

    tokio::spawn(async move {
        let stream = match source_fut.await {
            Ok(s) => s,
            Err(e) => {
                error!("debounced_command_stream: source init failed: {e:#}");
                return;
            }
        };
        tokio::pin!(stream);

        let mut deadline: Option<tokio::time::Instant> = None;

        loop {
            let sleep_until = deadline.unwrap_or_else(|| tokio::time::Instant::now() + FAR_FUTURE);

            tokio::select! {
                biased;

                _ = cancel.cancelled() => return,

                item = stream.next() => match item {
                    Some(_) => {
                        debug!("debounced_command_stream: event, (re)arming debounce");
                        deadline = Some(tokio::time::Instant::now() + silence);
                    }
                    None => {
                        error!("debounced_command_stream: source stream ended, feature will no longer receive events");
                        return;
                    }
                },

                _ = tokio::time::sleep_until(sleep_until), if deadline.is_some() => {
                    deadline = None;
                    debug!("debounced_command_stream: silence elapsed, emitting");
                    let req = CommandRequest {
                        command: Command::Interval(IntervalCommand {
                            feature_id: TypeId::of::<T>(),
                        }),
                        reply: None,
                    };
                    if tx.send(req).await.is_err() {
                        debug!("debounced_command_stream: receiver dropped, stopping");
                        return;
                    }
                }
            }
        }
    });

    tokio_stream::wrappers::ReceiverStream::new(rx).boxed()
}

pub fn direct_method_stream(rx: mpsc::Receiver<DirectMethod>) -> CommandRequestStream {
    tokio_stream::wrappers::ReceiverStream::new(rx)
        .filter_map(|dm| async move {
            match Command::from_direct_method(&dm) {
                Ok(command) => Some(CommandRequest {
                    command,
                    reply: Some(dm.responder),
                }),
                Err(e) => {
                    error!(
                        "parsing direct method: {} with payload: {} failed with error: {e:#}",
                        dm.name, dm.payload
                    );
                    if dm.responder.send(Err(e)).is_err() {
                        error!("direct method response receiver dropped")
                    }
                    None
                }
            }
        })
        .boxed()
}

pub fn desired_properties_stream(rx: mpsc::Receiver<TwinUpdate>) -> CommandRequestStream {
    tokio_stream::wrappers::ReceiverStream::new(rx)
        .filter_map(|twin| async move {
            let c: Vec<CommandRequest> = Command::from_desired_property(twin)
                .into_iter()
                .map(|command| CommandRequest {
                    command,
                    reply: None,
                })
                .collect();
            (!c.is_empty()).then_some(c)
        })
        .flat_map(stream::iter)
        .boxed()
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

    #[test]
    fn from_desired_property_ssh_tunnel_ca_pub_test() {
        // Valid ssh_tunnel_ca_pub
        let cmds = Command::from_desired_property(TwinUpdate {
            state: TwinUpdateState::Partial,
            value: json!({"ssh_tunnel_ca_pub": "ssh-ed25519 AAAA..."}),
        });
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Command::DesiredUpdateDeviceSshCa(_)));

        // Malformed ssh_tunnel_ca_pub — error is logged, no command produced
        let cmds = Command::from_desired_property(TwinUpdate {
            state: TwinUpdateState::Partial,
            value: json!({"ssh_tunnel_ca_pub": 42}),
        });
        assert!(cmds.is_empty());
    }

    #[tokio::test]
    async fn direct_method_stream_error_reply_test() {
        let (tx, rx) = mpsc::channel(16);
        let mut stream = direct_method_stream(rx);

        let (responder, reply_rx) = oneshot::channel::<CommandResult>();
        tx.send(DirectMethod {
            name: "nonexistent_method".to_string(),
            payload: json!({}),
            responder,
        })
        .await
        .expect("send direct method");

        // The stream should filter out the failed parse (return None)
        // and forward the error through the responder
        drop(tx);
        use futures::StreamExt;
        let next = stream.next().await;
        assert!(next.is_none(), "failed parse should be filtered out");

        // The error must have been forwarded to the responder
        let reply = reply_rx
            .await
            .expect("responder should have received error");
        assert!(reply.is_err());
    }

    #[tokio::test]
    async fn debounced_command_stream_basic_test() {
        let cancel = CancellationToken::new();
        let (source_tx, source_rx) = mpsc::channel::<()>(16);
        const SILENCE: Duration = Duration::from_millis(200);

        let mut stream = debounced_command_stream::<_, _, _, network::Network>(
            async { Ok(tokio_stream::wrappers::ReceiverStream::new(source_rx)) },
            SILENCE,
            cancel.clone(),
        );

        // Send one event
        source_tx.send(()).await.expect("send");
        // Wait for silence + margin
        tokio::time::sleep(Duration::from_millis(400)).await;

        use futures::StreamExt;
        let req = tokio::time::timeout(Duration::from_millis(200), stream.next())
            .await
            .expect("timeout waiting for debounced event")
            .expect("stream ended");
        assert!(matches!(req.command, Command::Interval(_)));

        cancel.cancel();
    }

    #[tokio::test]
    async fn debounced_command_stream_coalesces_events_test() {
        let cancel = CancellationToken::new();
        let (source_tx, source_rx) = mpsc::channel::<()>(16);
        const SILENCE: Duration = Duration::from_millis(200);

        let mut stream = debounced_command_stream::<_, _, _, network::Network>(
            async { Ok(tokio_stream::wrappers::ReceiverStream::new(source_rx)) },
            SILENCE,
            cancel.clone(),
        );

        // Send 3 events in rapid succession
        for _ in 0..3 {
            source_tx.send(()).await.expect("send");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Wait for debounce
        tokio::time::sleep(Duration::from_millis(400)).await;

        use futures::StreamExt;
        let req = tokio::time::timeout(Duration::from_millis(200), stream.next())
            .await
            .expect("timeout")
            .expect("stream ended");
        assert!(matches!(req.command, Command::Interval(_)));

        // No second event should arrive
        let second = tokio::time::timeout(Duration::from_millis(400), stream.next()).await;
        assert!(second.is_err(), "debounce should coalesce into one event");

        cancel.cancel();
    }

    #[tokio::test]
    async fn debounced_command_stream_cancellation_test() {
        let cancel = CancellationToken::new();
        let (_source_tx, source_rx) = mpsc::channel::<()>(16);
        const SILENCE: Duration = Duration::from_millis(200);

        let mut stream = debounced_command_stream::<_, _, _, network::Network>(
            async { Ok(tokio_stream::wrappers::ReceiverStream::new(source_rx)) },
            SILENCE,
            cancel.clone(),
        );

        // Cancel immediately
        cancel.cancel();

        use futures::StreamExt;
        // Stream should end (return None)
        let result = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
        assert!(
            matches!(result, Ok(None)),
            "stream should end after cancellation"
        );
    }

    #[tokio::test]
    async fn debounced_command_stream_source_init_failure_test() {
        let cancel = CancellationToken::new();
        const SILENCE: Duration = Duration::from_millis(200);

        let mut stream =
            debounced_command_stream::<_, futures::stream::Empty<()>, (), network::Network>(
                async { anyhow::bail!("D-Bus connection failed") },
                SILENCE,
                cancel,
            );

        use futures::StreamExt;
        // Stream should end immediately since the source future failed
        let result = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
        assert!(
            matches!(result, Ok(None)),
            "stream should end when source init fails"
        );
    }

    #[test]
    fn feature_id_routing_test() {
        use crate::twin::{
            consent::DeviceUpdateConsent, factory_reset, firmware_update::FirmwareUpdate,
            network::Network, reboot, ssh_tunnel::SshTunnel, system_info::SystemInfo,
        };
        use std::any::TypeId;

        // FsEvent routes to the feature_id embedded in the command
        let consent_id = TypeId::of::<DeviceUpdateConsent>();
        assert_eq!(
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileModified,
                feature_id: consent_id,
                path: PathBuf::from("/test"),
            })
            .feature_id(),
            consent_id
        );

        // Interval routes to the feature_id embedded in the command
        let network_id = TypeId::of::<Network>();
        assert_eq!(
            Command::Interval(IntervalCommand {
                feature_id: network_id,
            })
            .feature_id(),
            network_id
        );

        // Spot-check fixed-feature commands
        assert_eq!(Command::Reboot.feature_id(), TypeId::of::<reboot::Reboot>());
        assert_eq!(Command::ReloadNetwork.feature_id(), TypeId::of::<Network>());
        assert_eq!(
            Command::ValidateUpdate(true).feature_id(),
            TypeId::of::<FirmwareUpdate>()
        );
        assert_eq!(
            Command::FactoryReset(factory_reset::FactoryResetCommand {
                mode: factory_reset::FactoryResetMode::Mode1,
                preserve: vec![],
            })
            .feature_id(),
            TypeId::of::<factory_reset::FactoryReset>()
        );
        assert_eq!(
            Command::CloseSshTunnel(ssh_tunnel::CloseSshTunnelCommand {
                tunnel_id: "test".to_string(),
            })
            .feature_id(),
            TypeId::of::<SshTunnel>()
        );
        assert_eq!(
            Command::FleetId(system_info::FleetIdCommand {
                fleet_id: "test".to_string(),
            })
            .feature_id(),
            TypeId::of::<SystemInfo>()
        );
    }
}
