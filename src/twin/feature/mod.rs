use super::factory_reset::FactoryResetCommand;
use anyhow::{bail, Result};
use anyhow::{ensure, Context};
use async_trait::async_trait;
use azure_iot_sdk::client::DirectMethod;
use azure_iot_sdk::client::IotMessage;
use futures::Stream;
use futures::StreamExt;
use log::{debug, error};
use notify_debouncer_full::{new_debouncer, notify::*, DebounceEventResult, Debouncer, NoCache};
use std::any::{Any, TypeId};
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;
use tokio::{
    sync::mpsc,
    time::{Instant, Interval},
};

#[derive(Debug, Eq, PartialEq, Hash)]
pub enum Command {
    FactoryReset(FactoryResetCommand),
    Reboot,
    ReloadNetwork,
}

impl Command {
    pub fn feature_id(self) -> TypeId {
        use Command::*;

        match self {
            FactoryReset(_) => TypeId::of::<super::FactoryReset>(),
            Reboot => TypeId::of::<super::Reboot>(),
            ReloadNetwork => TypeId::of::<super::Network>(),
        }
    }
    pub fn from_direct_method(direct_method: &DirectMethod) -> Result<Command> {
        match direct_method.name.as_str() {
            "factory_reset" => Ok(Command::FactoryReset(
                serde_json::from_value(direct_method.payload.clone())
                    .context("cannot parse FactoryResetCommand from direct method payload")?,
            )),

            /*         "user_consent" => self
                .feature::<DeviceUpdateConsent>()?
                .user_consent(method.payload),
            "get_ssh_pub_key" => {
                self.feature::<SshTunnel>()?
                    .get_ssh_pub_key(method.payload)
                    .await
            }
            "open_ssh_tunnel" => {
                self.feature::<SshTunnel>()?
                    .open_ssh_tunnel(method.payload)
                    .await
            }
            "close_ssh_tunnel" => {
                self.feature::<SshTunnel>()?
                    .close_ssh_tunnel(method.payload)
                    .await
            }
            "reboot" => self.feature::<Reboot>()?.reboot().await,
            "set_wait_online_timeout" => {
                self.feature::<Reboot>()?
                    .set_wait_online_timeout(method.payload)
                    .await */
            _ => bail!(
                "cannot parse direct method {} with payload {}",
                direct_method.name,
                direct_method.payload
            ),
        }
    }
}

#[async_trait(?Send)]
pub(crate) trait Feature {
    fn name(&self) -> String;

    fn version(&self) -> u8;

    fn is_enabled(&self) -> bool;

    fn as_any(&self) -> &dyn Any;

    fn ensure(&self) -> Result<()> {
        if !self.is_enabled() {
            bail!("feature disabled: {}", self.name());
        }

        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        unimplemented!();
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

    fn register_command(&self) -> &'static str {
        unimplemented!();
    }

    async fn command(&self, _cmd: &Command) -> Result<Option<serde_json::Value>> {
        unimplemented!();
    }
}

#[derive(Debug)]
pub enum EventData {
    FileCreated(PathBuf),
    FileModified(PathBuf),
    Interval(Instant),
    Manual,
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
                    debug!("notify-event: {de:#?}");
                    if let EventKind::Modify(_) = de.event.kind {
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
