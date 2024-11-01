use anyhow::ensure;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use futures::Stream;
use futures::StreamExt;
use log::{debug, error, info, warn};
use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::any::{Any, TypeId};
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;
use tokio::time::{Instant, Interval};
use tokio::{select, sync::mpsc};

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

    fn refresh_event(&self) -> Result<Option<StreamResult>> {
        Ok(None)
    }

    async fn refresh(&mut self, _reason: &EventData) -> Result<()> {
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

pub type StreamResult = Pin<Box<dyn Stream<Item = Event> + Send>>;

pub fn interval_stream_option(interval: Option<Interval>) -> impl Stream {
    match interval {
        None => futures_util::stream::empty::<Instant>().boxed(),
        Some(interval) => tokio_stream::wrappers::IntervalStream::new(interval).boxed(),
    }
}

pub fn interval_stream<T>(interval: Interval) -> StreamResult
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

pub fn file_created_stream<T>(paths: Vec<&Path>) -> StreamResult
where
    T: 'static,
{
    let (tx, rx) = mpsc::channel(2);
    let inner_paths: Vec<PathBuf> = paths.into_iter().map(|p| p.to_path_buf()).collect();

    tokio::task::spawn_blocking(move || loop {
        for p in &inner_paths {
            if matches!(p.try_exists(), Ok(true)) {
                tx.blocking_send(p.clone()).unwrap();
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

pub fn file_modified_stream<T>(paths: Vec<&Path>) -> Result<StreamResult>
where
    T: 'static,
{
    let (tx, rx) = mpsc::channel(2);
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            //Ok(ev) if ev.kind == notify::EventKind::Modify(notify::event::ModifyKind::Any) => {
            Ok(ev) => {
                debug!("notify-event: {ev:#?}");
                for path in ev.paths {
                    tx.blocking_send(path).unwrap();
                }
            }
            Err(errors) => errors
                .paths
                .iter()
                .for_each(|e| error!("notify-error: {e:?}")),
        })?;

    for p in paths {
        ensure!(p.is_file(), "{p:?} is not a regular existing file");
        watcher.watch(p, RecursiveMode::NonRecursive)?;
    }

    Ok(tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|p| Event {
            feature_id: TypeId::of::<T>(),
            data: EventData::FileModified(p),
        })
        .boxed())
}
