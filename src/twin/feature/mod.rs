use super::consent::{DesiredGeneralConsentCommand, UserConsentCommand};
use super::factory_reset::FactoryResetCommand;
use super::reboot::SetWaitOnlineTimeoutCommand;
use super::ssh_tunnel::{CloseSshTunnelCommand, GetSshPubKeyCommand, OpenSshTunnelCommand};
use super::{TwinUpdate, TwinUpdateState};
use anyhow::{bail, Result};
use anyhow::{ensure, Context};
use async_trait::async_trait;
use azure_iot_sdk::client::DirectMethod;
use azure_iot_sdk::client::IotMessage;
use futures::Stream;
use futures::StreamExt;
use log::{debug, error, warn};
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

#[derive(Debug)]
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

    pub fn from_direct_method(direct_method: &DirectMethod) -> Result<Command> {
        match direct_method.name.as_str() {
            "factory_reset" => Ok(Command::FactoryReset(
                serde_json::from_value(direct_method.payload.clone())
                    .context("cannot parse FactoryResetCommand from direct method payload")?,
            )),
            "user_consent" => Ok(Command::UserConsent(
                serde_json::from_value(direct_method.payload.clone())
                    .context("cannot parse UserConsentCommand from direct method payload")?,
            )),
            "get_ssh_pub_key" => Ok(Command::GetSshPubKey(
                serde_json::from_value(direct_method.payload.clone())
                    .context("cannot parse GetSshPubKeyCommand from direct method payload")?,
            )),
            "open_ssh_tunnel" => Ok(Command::OpenSshTunnel(
                serde_json::from_value(direct_method.payload.clone())
                    .context("cannot parse OpenSshTunnelCommand from direct method payload")?,
            )),
            "close_ssh_tunnel" => Ok(Command::CloseSshTunnel(
                serde_json::from_value(direct_method.payload.clone())
                    .context("cannot parse CloseSshTunnelCommand from direct method payload")?,
            )),
            "reboot" => Ok(Command::Reboot),
            "set_wait_online_timeout" => Ok(Command::SetWaitOnlineTimeout(
                serde_json::from_value(direct_method.payload.clone()).context(
                    "cannot parse SetWaitOnlineTimeoutCommand from direct method payload",
                )?,
            )),
            _ => bail!(
                "cannot parse direct method {} with payload {}",
                direct_method.name,
                direct_method.payload
            ),
        }
    }

    pub fn from_desired_property(update: TwinUpdate) -> Result<Option<Command>> {
        debug!("desired property: {update:?}");

        match update.state {
            TwinUpdateState::Partial => {
                if let Some(gc) = update.value.get("general_consent") {
                    return  Ok(Some(Command::DesiredGeneralConsent(
                        serde_json::from_value(gc.clone()).context(format!(
                            "from_desired_property: cannot parse DesiredGeneralConsentCommand: {gc:#?}"
                        ))?,
                    )));
                }
            }
            TwinUpdateState::Complete => {
                let gc = &update.value["desired"]["general_consent"];
                if !gc.is_null() {
                    return Ok(Some(Command::DesiredGeneralConsent(
                            serde_json::from_value(gc.clone()).context(format!(
                                "from_desired_property: cannot parse DesiredGeneralConsentCommand: {gc:#?}"
                            ))?,
                        )));
                }
                warn!("from_desired_property: 'desired: general_consent' missing while TwinUpdateState::Complete");
            }
        }
        Ok(None)
    }
}

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

    async fn handle_event(&mut self, _event: &EventData) -> Result<()> {
        unimplemented!();
    }

    async fn command(&mut self, _cmd: Command) -> Result<Option<serde_json::Value>> {
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
