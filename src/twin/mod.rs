mod consent;
mod factory_reset;
pub mod feature;
mod firmware_update;
#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;
mod modem_info;
mod network;
mod provisioning_config;
mod reboot;
mod ssh_tunnel;
mod system_info;
mod wifi_commissioning;

#[cfg(test)]
use {
    mod_test::mod_test::MockMyIotHub as IotHubClient,
    mod_test::mod_test::MyIotHubBuilder as IotHubClientBuilder,
};

#[cfg(not(test))]
use azure_iot_sdk::client::{IotHubClient, IotHubClientBuilder};

use crate::{systemd, twin::feature::*, web_service};
use anyhow::{Context, Result, bail};
use azure_iot_sdk::client::*;
use dotenvy;
use futures::future::OptionFuture;
use futures_util::StreamExt;
use log::{error, info, warn};
use serde_json::json;
use signal_hook::consts::TERM_SIGNALS;
use signal_hook_tokio::Signals;
use std::{
    any::TypeId,
    collections::HashMap,
    env,
    path::Path,
    time::{self, Duration},
};
use tokio::{select, sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::{IntervalStream, ReceiverStream};
use tokio_util::sync::CancellationToken;

#[derive(PartialEq)]
enum TwinState {
    Uninitialized,
    Initialized,
    Authenticated,
}

pub struct Twin {
    client: Option<IotHubClient>,
    web_service: Option<web_service::WebService>,
    tx_command_request: mpsc::Sender<CommandRequest>,
    tx_reported_properties: mpsc::Sender<serde_json::Value>,
    tx_outgoing_message: mpsc::Sender<IotMessage>,
    state: TwinState,
    features: HashMap<TypeId, Box<DynFeature<'static>>>,
    waiting_for_reboot: bool,
    cancel: CancellationToken,
    fs_watcher_handle: Option<JoinHandle<()>>,
}

impl Twin {
    async fn new(
        tx_command_request: mpsc::Sender<CommandRequest>,
        tx_reported_properties: mpsc::Sender<serde_json::Value>,
        tx_outgoing_message: mpsc::Sender<IotMessage>,
    ) -> Result<Self> {
        let web_service = web_service::WebService::run(tx_command_request.clone()).await?;
        /*
            - init features first
            - start with SystemInfo in order to log useful infos asap
        */
        #[cfg(not(feature = "mock"))]
        let mut fs_watcher = FsWatcher::new()?;
        #[cfg(feature = "mock")]
        let mut fs_watcher = FsWatcher::noop();
        let features = HashMap::from([
            (
                TypeId::of::<system_info::SystemInfo>(),
                DynFeature::new_box(system_info::SystemInfo::new(&mut fs_watcher)?),
            ),
            (
                TypeId::of::<consent::DeviceUpdateConsent>(),
                DynFeature::new_box(consent::DeviceUpdateConsent::new(&mut fs_watcher)?),
            ),
            (
                TypeId::of::<factory_reset::FactoryReset>(),
                DynFeature::new_box(factory_reset::FactoryReset::new(&mut fs_watcher)?),
            ),
            (
                TypeId::of::<firmware_update::FirmwareUpdate>(),
                DynFeature::new_box(firmware_update::FirmwareUpdate::new().await?),
            ),
            (
                TypeId::of::<modem_info::ModemInfo>(),
                DynFeature::new_box(modem_info::ModemInfo::new()),
            ),
            (
                TypeId::of::<network::Network>(),
                DynFeature::new_box(network::Network::default()),
            ),
            (
                TypeId::of::<provisioning_config::ProvisioningConfig>(),
                DynFeature::new_box(provisioning_config::ProvisioningConfig::new()?),
            ),
            (
                TypeId::of::<reboot::Reboot>(),
                DynFeature::new_box(reboot::Reboot::default()),
            ),
            (
                TypeId::of::<ssh_tunnel::SshTunnel>(),
                DynFeature::new_box(ssh_tunnel::SshTunnel::new()),
            ),
            (
                TypeId::of::<wifi_commissioning::WifiCommissioning>(),
                DynFeature::new_box(wifi_commissioning::WifiCommissioning::default()),
            ),
        ]);

        let cancel = CancellationToken::new();
        let fs_watcher_handle =
            fs_watcher.into_stream(tx_command_request.clone(), cancel.child_token())?;

        let twin = Twin {
            client: None,
            web_service,
            tx_command_request,
            tx_reported_properties,
            tx_outgoing_message,
            state: TwinState::Uninitialized,
            features,
            waiting_for_reboot: false,
            cancel,
            fs_watcher_handle,
        };

        twin.connect_web_service().await?;

        Ok(twin)
    }

    fn feature_info(&self) -> serde_json::Value {
        let mut features = serde_json::Map::new();

        // report feature availability and version
        for f in self.features.values() {
            let value = if f.is_enabled() {
                json!({ "version": f.version() })
            } else {
                json!(null)
            };

            features.insert(f.name(), value);
        }

        serde_json::Value::Object(features)
    }

    async fn connect_twin(&mut self) -> Result<()> {
        let client = self.client.as_ref().context("client not present")?;

        // report feature availability and version
        client.twin_report(self.feature_info())?;

        // connect twin channels
        for f in self.features.values_mut() {
            if f.is_enabled() {
                f.connect_twin(
                    self.tx_reported_properties.clone(),
                    self.tx_outgoing_message.clone(),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn connect_web_service(&self) -> Result<()> {
        web_service::publish(
            web_service::PublishChannel::OnlineStatusV1,
            json!({"iothub": false}),
        )
        .await;

        // connect twin channels
        for f in self.features.values() {
            if f.is_enabled() {
                f.connect_web_service().await?;
            }
        }

        Ok(())
    }

    async fn handle_connection_status(
        &mut self,
        auth_status: AuthenticationStatus,
    ) -> Result<bool> {
        let mut restart_twin = false;
        match auth_status {
            AuthenticationStatus::Authenticated => {
                if self.state != TwinState::Authenticated {
                    info!("Succeeded to connect to iothub");

                    self.connect_twin().await?;

                    self.state = TwinState::Authenticated;
                }
            }
            AuthenticationStatus::Unauthenticated(reason) => {
                if self.state == TwinState::Authenticated {
                    self.state = TwinState::Initialized;
                }

                match reason {
                    UnauthenticatedReason::BadCredential
                    | UnauthenticatedReason::CommunicationError => {
                        error!(
                            "Failed to connect to iothub: {reason:?}. Possible reasons: certificate renewal, reprovisioning or wrong system time"
                        );

                        /*
                        here we start all over again. reason: there are situations where we get
                        a wrong connection string (BadCredential) from identity service due to wrong system time or
                        certificate renewal caused by iot-identity-service reprovisioning.
                        this may occur on devices without RTC or where time is not synced. since we experienced this
                        behavior only for a moment after boot (e.g. RPI without rtc) we just try again.
                        */

                        restart_twin = true;
                    }
                    UnauthenticatedReason::RetryExpired
                    | UnauthenticatedReason::ExpiredSasToken
                    | UnauthenticatedReason::NoNetwork
                    | UnauthenticatedReason::Unknown => {
                        info!("Failed to connect to iothub: {reason:?}");
                    }
                    UnauthenticatedReason::DeviceDisabled => {
                        warn!("Failed to connect to iothub: {reason:?}")
                    }
                }
            }
        }

        let authenticated = auth_status == AuthenticationStatus::Authenticated;

        self.request_validate_update(authenticated).await?;

        web_service::publish(
            web_service::PublishChannel::OnlineStatusV1,
            json!({"iothub": authenticated}),
        )
        .await;

        Ok(restart_twin)
    }

    async fn handle_request(&mut self, request: CommandRequest) -> Result<()> {
        let cmd = request.command;
        let reply = request.reply;
        let cmd_string = format!("{cmd:?}");

        let feature = self
            .features
            .get_mut(&cmd.feature_id())
            .context("handle_request: failed to get feature mutable")?;

        if !feature.is_enabled() {
            // FsEvents and intervals have no reply channel — silently drop them
            // for disabled features instead of erroring. Direct methods and web
            // service requests carry a reply channel and should still fail loudly.
            if reply.is_none() {
                info!(
                    "handle_request: dropping command for disabled feature {}",
                    feature.name()
                );
                return Ok(());
            }
            bail!("handle_request: feature is disabled {}", feature.name());
        }

        info!("handle_request: {}({cmd_string})", feature.name());

        let result = feature.command(&cmd).await;

        match &result {
            Ok(inner_result) => {
                info!("handle_request: {cmd_string} succeeded with result: {inner_result:?}")
            }
            Err(e) => error!("handle_request: {cmd_string} returned error: {e:#}"),
        }

        if let Some(reply) = reply
            && reply.send(result).is_err()
        {
            error!("handle_request: {cmd_string} receiver dropped");
        }

        #[cfg(not(feature = "mock"))]
        if cmd.triggers_reboot() {
            self.waiting_for_reboot = true;
        }

        Ok(())
    }

    // Without FsWatcher, feature commands triggered by file changes are lost
    // silently. Shut down and surface an error so main exits non-zero and
    // systemd restarts the service with a fresh watcher. If the task exited
    // because we cancelled it (normal shutdown), return Ok — the watcher
    // ending was expected and another branch owns the exit code.
    async fn handle_fs_watcher_exit(
        &mut self,
        result: Result<(), tokio::task::JoinError>,
        rx_reported_properties: &mut mpsc::Receiver<serde_json::Value>,
        rx_outgoing_message: &mut mpsc::Receiver<IotMessage>,
    ) -> Result<()> {
        if self.cancel.is_cancelled() {
            info!("FsWatcher task exited after cancellation");
            return Ok(());
        }
        match result {
            Ok(()) => error!("FsWatcher task exited, all file watches are now inactive"),
            Err(e) => error!("FsWatcher task panicked: {e}"),
        }
        self.shutdown(rx_reported_properties, rx_outgoing_message)
            .await;
        bail!("FsWatcher task ended; exiting to let systemd restart the service")
    }

    async fn shutdown(
        &mut self,
        rx_reported_properties: &mut mpsc::Receiver<serde_json::Value>,
        rx_outgoing_message: &mut mpsc::Receiver<IotMessage>,
    ) {
        info!("shutdown");

        self.cancel.cancel();

        if let Some(client) = self.client.as_mut() {
            // report remaining properties
            while let Ok(reported) = rx_reported_properties.try_recv() {
                client
                    .twin_report(reported)
                    .unwrap_or_else(|e| error!("couldn't report while shutting down: {e:#}"));
            }

            // send remaining messages
            while let Ok(message) = rx_outgoing_message.try_recv() {
                client
                    .send_d2c_message(message)
                    .unwrap_or_else(|e| error!("couldn't send while shutting down: {e:#}"));
            }

            client.shutdown(Duration::from_secs(5)).await;
            self.client = None;
        }

        if let Some(ws) = &self.web_service {
            ws.shutdown().await;
        }
        info!("twin shutdown complete");
    }

    async fn reset_client_with_delay(&mut self, timeout: Option<time::Duration>) {
        if let Some(client) = self.client.as_mut() {
            info!("reset_client: shutdown iotclient");
            client.shutdown(Duration::from_secs(5)).await;
            self.client = None;
        }

        if let Some(t) = timeout {
            info!("reset_client: sleep for {}ms", t.as_millis());
            tokio::time::sleep(t).await;
        }
    }

    fn feature_command_request_streams(&mut self) -> Vec<CommandRequestStream> {
        self.features
            .values_mut()
            .filter_map(|f| {
                if !f.is_enabled() {
                    return None;
                }
                match f.command_request_stream(self.cancel.child_token()) {
                    Ok(stream) => stream,
                    Err(e) => {
                        error!(
                            "feature_command_request_streams: {} failed: {e:#}",
                            f.name()
                        );
                        None
                    }
                }
            })
            .collect()
    }

    async fn request_validate_update(&mut self, authenticated: bool) -> Result<()> {
        self.tx_command_request
            .send(CommandRequest {
                command: Command::ValidateUpdate(authenticated),
                reply: None,
            })
            .await
            .context("request_validate_update: requests receiver dropped")
    }

    pub async fn run() -> Result<()> {
        let (tx_connection_status, mut rx_connection_status) = mpsc::channel(100);
        let (tx_twin_desired, rx_twin_desired) = mpsc::channel(100);
        let (tx_direct_method, rx_direct_method) = mpsc::channel(100);
        let (tx_reported_properties, mut rx_reported_properties) = mpsc::channel(100);
        let (tx_outgoing_message, mut rx_outgoing_message) = mpsc::channel(100);
        let (tx_command_request, rx_command_request) = mpsc::channel(100);

        // load env vars from /usr/lib/os-release, e.g. to determine feature availability
        dotenvy::from_path_override(
            Path::new(&env::var("OS_RELEASE_DIR_PATH").unwrap_or("/usr/lib".to_string()))
                .join("os-release"),
        )?;

        let mut twin = Self::new(
            tx_command_request,
            tx_reported_properties,
            tx_outgoing_message,
        )
        .await?;

        let client_builder = IotHubClient::builder()
            .observe_connection_state(tx_connection_status)
            .observe_desired_properties(tx_twin_desired)
            .observe_direct_methods(tx_direct_method);

        let mut signals = Signals::new(TERM_SIGNALS)?;

        systemd::sd_notify_ready();

        let mut command_requests = twin.feature_command_request_streams();
        command_requests.push(direct_method_stream(rx_direct_method));
        command_requests.push(desired_properties_stream(rx_twin_desired));
        command_requests.push(ReceiverStream::new(rx_command_request).boxed());

        let mut fs_watcher_handle = twin.fs_watcher_handle.take();

        tokio::pin! {
            let client_created = Self::connect_iothub_client(&client_builder);
            let trigger_watchdog = match systemd::watchdog::WatchdogManager::init().await {
                None => futures_util::stream::empty::<tokio::time::Instant>().boxed(),
                Some(interval) => IntervalStream::new(interval).boxed(),
            };
            let command_requests = futures::stream::select_all::select_all(command_requests);
        };

        loop {
            select! (
                // we enforce top down order to handle select! branches
                biased;

                Some(_) = trigger_watchdog.next() => {
                    systemd::watchdog::WatchdogManager::notify().await?;
                },
                Some(signal) = signals.next() => {
                    info!("received termination signal: {signal:?}");
                    twin.shutdown(&mut rx_reported_properties, &mut rx_outgoing_message).await;
                    signals.handle().close();
                    return Ok(())
                },
                Some(result) = OptionFuture::from(fs_watcher_handle.as_mut()) => {
                    signals.handle().close();
                    return twin
                        .handle_fs_watcher_exit(
                            result,
                            &mut rx_reported_properties,
                            &mut rx_outgoing_message,
                        )
                        .await;
                },
                result = &mut client_created, if twin.client.is_none() => {
                    match result {
                        Ok(client) => {
                            info!("iothub client created");
                            twin.client = Some(client);
                        },
                        Err(e) => {
                            error!("couldn't create iothub client: {e:#}");
                            twin.request_validate_update(false).await?;
                            twin.reset_client_with_delay(Some(time::Duration::from_secs(10))).await;
                            client_created.set(Self::connect_iothub_client(&client_builder));
                        }
                    }
                },
                Some(status) = rx_connection_status.recv() => {
                    if twin.handle_connection_status(status).await? {
                        twin.reset_client_with_delay(Some(time::Duration::from_secs(1))).await;
                        client_created.set(Self::connect_iothub_client(&client_builder));
                    };
                },
                requests = command_requests.select_next_some(), if !twin.waiting_for_reboot => {
                    twin.handle_request(requests).await?
                },
                Some(reported) = rx_reported_properties.recv() => {
                    let Some(client) = &twin.client else {
                        error!("couldn't report properties since client not present");
                        continue
                    };

                    client.twin_report(reported)?
                },
                Some(message) = rx_outgoing_message.recv() => {
                    let Some(client) = &twin.client else {
                        error!("couldn't send msg since client not present");
                        continue
                    };

                    client.send_d2c_message(message)?
                },
            );
        }
    }

    #[cfg(not(feature = "mock"))]
    async fn connect_iothub_client(builder: &IotHubClientBuilder) -> Result<IotHubClient> {
        info!("start client and wait for authentication...");

        builder.build_module_client_from_identity().await
    }

    #[cfg(feature = "mock")]
    async fn connect_iothub_client(builder: &IotHubClientBuilder) -> Result<IotHubClient> {
        info!("start client and wait for authentication...");

        builder.build_module_client(
            &env::var("CONNECTION_STRING").context("connection string missing")?,
        )
    }
}
