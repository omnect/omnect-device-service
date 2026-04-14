mod command;
mod fs_watcher;
pub use command::*;
pub use fs_watcher::*;
pub use tokio_util::sync::CancellationToken;

use anyhow::Result;
use azure_iot_sdk::client::IotMessage;
use tokio::sync::mpsc;

#[dynosaur::dynosaur(pub DynFeature = dyn(box) Feature, bridge(none))]
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

    fn command_request_stream(&mut self, _cancel: CancellationToken) -> CommandRequestStreamResult {
        Ok(None)
    }

    async fn command(&mut self, _cmd: &Command) -> CommandResult {
        unimplemented!();
    }
}
