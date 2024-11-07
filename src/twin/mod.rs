mod consent;
pub(crate) mod factory_reset;
pub(crate) mod feature;
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

cfg_if::cfg_if! {
    if #[cfg(test)] {
        use mod_test::mod_test::MockMyIotHub as IotHubClient;
        use mod_test::mod_test::MyIotHubBuilder as IotHubClientBuilder;
    } else {
        use azure_iot_sdk::client::{IotHubClient, IotHubClientBuilder};
    }
}

use crate::twin::{
    consent::DeviceUpdateConsent, factory_reset::FactoryReset, feature::*, modem_info::ModemInfo,
    network::Network, provisioning_config::ProvisioningConfig, reboot::Reboot,
    ssh_tunnel::SshTunnel, system_info::SystemInfo, wifi_commissioning::WifiCommissioning,
};
use crate::web_service::{self, PublishChannel, Request as WebServiceCommand, WebService};
use crate::{systemd, systemd::watchdog::WatchdogManager};
use crate::{update_validation, update_validation::UpdateValidation};

use anyhow::{ensure, Context, Result};
use azure_iot_sdk::client::{
    AuthenticationStatus, IotMessage, TwinUpdate, TwinUpdateState, UnauthenticatedReason,
};
use dotenvy;
use futures_executor::block_on;
use futures_util::StreamExt;
use log::{error, info, warn};
use serde_json::json;
use signal_hook::consts::TERM_SIGNALS;
use signal_hook_tokio::Signals;
use std::{any::TypeId, collections::HashMap, path::Path, time};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};

#[derive(PartialEq)]
enum TwinState {
    Uninitialized,
    Initialized,
    Authenticated,
}

pub struct Twin {
    client: Option<IotHubClient>,
    web_service: Option<WebService>,
    tx_reported_properties: mpsc::Sender<serde_json::Value>,
    tx_outgoing_message: mpsc::Sender<IotMessage>,
    state: TwinState,
    features: HashMap<TypeId, Box<dyn Feature>>,
    update_validation: update_validation::UpdateValidation,
}

impl Twin {
    async fn new(
        tx_web_service: mpsc::Sender<WebServiceCommand>,
        tx_reported_properties: mpsc::Sender<serde_json::Value>,
        tx_outgoing_message: mpsc::Sender<IotMessage>,
    ) -> Result<Self> {
        // has to be called before iothub client authentication
        let update_validation = UpdateValidation::new()?;
        let client = None;
        let web_service = WebService::run(tx_web_service.clone()).await?;
        let state = TwinState::Uninitialized;

        let features = HashMap::from([
            (
                TypeId::of::<DeviceUpdateConsent>(),
                Box::<DeviceUpdateConsent>::default() as Box<dyn Feature>,
            ),
            (
                TypeId::of::<FactoryReset>(),
                Box::new(FactoryReset::new()) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<ModemInfo>(),
                Box::new(ModemInfo::new()) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<Network>(),
                Box::<Network>::default() as Box<dyn Feature>,
            ),
            (
                TypeId::of::<ProvisioningConfig>(),
                Box::new(ProvisioningConfig::new()?) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<Reboot>(),
                Box::<Reboot>::default() as Box<dyn Feature>,
            ),
            (
                TypeId::of::<SshTunnel>(),
                Box::new(SshTunnel::new()) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<SystemInfo>(),
                Box::new(SystemInfo::new()?) as Box<dyn Feature>,
            ),
            (
                TypeId::of::<WifiCommissioning>(),
                Box::<WifiCommissioning>::default() as Box<dyn Feature>,
            ),
        ]);

        let twin = Twin {
            client,
            web_service,
            tx_reported_properties,
            tx_outgoing_message,
            state,
            features,
            update_validation,
        };

        twin.connect_web_service().await?;

        Ok(twin)
    }

    async fn connect_twin(&mut self) -> Result<()> {
        let client = self.client.as_ref().context("client not present")?;

        // report feature availability
        for f in self.features.values() {
            let value = if f.is_enabled() {
                json!({ "version": f.version() })
            } else {
                json!(null)
            };

            client.twin_report(json!({ f.name(): value }))?;
        }

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
        web_service::publish(PublishChannel::OnlineStatus, json!({"iothub": false})).await?;

        // connect twin channels
        for f in self.features.values() {
            if f.is_enabled() {
                f.connect_web_service().await?;
            }
        }

        Ok(())
    }

    fn handle_connection_status(&mut self, auth_status: AuthenticationStatus) -> Result<bool> {
        block_on(async {
            let mut restart_twin = false;
            match auth_status {
                AuthenticationStatus::Authenticated => {
                    if self.state != TwinState::Authenticated {
                        info!("Succeeded to connect to iothub");

                        if self.state == TwinState::Uninitialized {
                            /*
                             * the update validation test "wait_for_system_running" enforces that
                             * omnect-device-service already notified its own success
                             */
                            self.update_validation.set_authenticated().await?;
                        };

                        self.connect_twin().await?;

                        self.state = TwinState::Authenticated;
                    }
                }
                AuthenticationStatus::Unauthenticated(reason) => {
                    if matches!(self.state, TwinState::Authenticated) {
                        self.state = TwinState::Initialized;
                    }

                    match reason {
                        UnauthenticatedReason::BadCredential
                        | UnauthenticatedReason::CommunicationError => {
                            error!("Failed to connect to iothub: {reason:?}. Possible reasons: certificate renewal, reprovisioning or wrong system time");

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
                            info!("Failed to connect to iothub: {reason:?}")
                        }
                        UnauthenticatedReason::DeviceDisabled => {
                            warn!("Failed to connect to iothub: {reason:?}")
                        }
                    }
                }
            }

            web_service::publish(
                PublishChannel::OnlineStatus,
                json!({"iothub": auth_status == AuthenticationStatus::Authenticated}),
            )
            .await?;

            Ok(restart_twin)
        })
    }

    fn handle_command(
        &mut self,
        cmd: Command,
        reply: Option<oneshot::Sender<CommandResult>>,
    ) -> Result<()> {
        block_on(async {
            let cmd_string = format!("{cmd:?}");

            let feature = self
                .features
                .get_mut(&cmd.feature_id())
                .ok_or_else(|| anyhow::anyhow!("failed to get feature mutable"))?;

            ensure!(
                feature.is_enabled(),
                "handle_command: feature is disabled {}",
                feature.name()
            );

            info!("handle_command: {}({cmd_string})", feature.name());

            let result = feature.command(cmd).await;

            match &result {
                Ok(inner_result) => {
                    info!("handle_command: {cmd_string} succeeded with result: {inner_result:?}")
                }
                Err(e) => error!("handle_command: {cmd_string} returned error: {e}"),
            }

            if let Some(reply) = reply {
                if reply.send(result).is_err() {
                    error!("handle_command: {cmd_string} receiver dropped");
                }
            }

            Ok(())
        })
    }

    fn shutdown(
        &mut self,
        rx_reported_properties: &mut mpsc::Receiver<serde_json::Value>,
        rx_outgoing_message: &mut mpsc::Receiver<IotMessage>,
    ) {
        block_on(async {
            info!("shutdown");

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

                client.shutdown().await;
                self.client = None;
            }

            if let Some(ws) = &self.web_service {
                ws.shutdown().await;
            }
        })
    }

    fn reset_client_with_delay(&mut self, timeout: Option<time::Duration>) {
        block_on(async {
            if let Some(client) = self.client.as_mut() {
                info!("reset_client: shutdown iotclient");
                client.shutdown().await;
                self.client = None;
            }

            if let Some(t) = timeout {
                info!("reset_client: sleep for {}ms", t.as_millis());
                tokio::time::sleep(t).await
            }
        })
    }

    pub async fn run() -> Result<()> {
        let (tx_connection_status, mut rx_connection_status) = mpsc::channel(100);
        let (tx_twin_desired, mut rx_twin_desired) = mpsc::channel(100);
        let (tx_direct_method, mut rx_direct_method) = mpsc::channel(100);
        let (tx_reported_properties, mut rx_reported_properties) = mpsc::channel(100);
        let (tx_outgoing_message, mut rx_outgoing_message) = mpsc::channel(100);
        let (tx_web_service, mut rx_web_service) = mpsc::channel(100);

        // load env vars from /usr/lib/os-release, e.g. to determine feature availability
        dotenvy::from_path_override(Path::new(&format!(
            "{}/os-release",
            std::env::var("OS_RELEASE_DIR_PATH").unwrap_or_else(|_| "/usr/lib".to_string())
        )))?;

        let mut twin =
            Self::new(tx_web_service, tx_reported_properties, tx_outgoing_message).await?;

        let client_builder = IotHubClient::builder()
            .observe_connection_state(tx_connection_status)
            .observe_desired_properties(tx_twin_desired)
            .observe_direct_methods(tx_direct_method);

        let mut signals = Signals::new(TERM_SIGNALS)?;

        tokio::pin! {
            let client_created = Self::connect_iothub_client(&client_builder);
            let trigger_watchdog = match WatchdogManager::init(){
                None => futures_util::stream::empty::<tokio::time::Instant>().boxed(),
                Some(interval) => tokio_stream::wrappers::IntervalStream::new(interval).boxed(),
            };
            let refresh_features = futures::stream::select_all::select_all(twin
                .features
                .values_mut()
                .filter_map(|f| f.is_enabled().then(|| f.event_stream().unwrap().unwrap())));
        };

        systemd::sd_notify_ready();

        loop {
            select! (
                // we enforce top down order in 1st select! macro to handle sd-notify, signals and connections status
                // with priority over events in the 2nd select!
                biased;

                Some(_) = trigger_watchdog.next() => {
                    WatchdogManager::notify()?;
                },
                _ = signals.next() => {
                    twin.shutdown(&mut rx_reported_properties, &mut rx_outgoing_message);
                    signals.handle().close();
                    return Ok(())
                },
                result = &mut client_created, if twin.client.is_none() => {
                    match result {
                        Ok(client) => {
                            info!("iothub client created");
                            twin.client = Some(client);
                        },
                        Err(e) => {
                            error!("couldn't create iothub client: {e:#}");
                            twin.reset_client_with_delay(Some(time::Duration::from_secs(10)));
                            client_created.set(Self::connect_iothub_client(&client_builder));
                        }
                    }
                },
                Some(status) = rx_connection_status.recv() => {
                    if twin.handle_connection_status(status)? {
                        twin.reset_client_with_delay(Some(time::Duration::from_secs(1)));
                        client_created.set(Self::connect_iothub_client(&client_builder));
                    };
                },
                result = async {
                    select! (
                        // random access order in 2nd select! macro
                        Some(update_desired) = rx_twin_desired.recv() => {
                            for cmd in Command::from_desired_property(update_desired) {
                                twin.handle_command(cmd, None)?
                            }
                        },
                        Some(reported) = rx_reported_properties.recv() => {
                            twin.client
                                .as_ref()
                                .context("couldn't report properties since client not present")?
                                .twin_report(reported)?
                        },
                        Some(direct_method) = rx_direct_method.recv() => {
                            if let Some(cmd) = Command::from_direct_method(&direct_method) {
                                twin.handle_command(cmd, Some(direct_method.responder))?
                            }
                        },
                        Some(message) = rx_outgoing_message.recv() => {
                            twin.client
                                .as_ref()
                                .context("couldn't send msg since client not present")?
                                .send_d2c_message(message)?
                        },
                        Some(request) = rx_web_service.recv() => {
                            twin.handle_command(request.command, Some(request.reply))?
                        },
                        command = refresh_features.select_next_some() => {
                            let feature = twin
                                .features
                                .get_mut(&command.feature_id())
                                .ok_or_else(|| anyhow::anyhow!("failed to get feature mutable"))?;

                            ensure!(feature.is_enabled(), "event stream: feature is disabled {}", feature.name());
                            info!("event stream: {}({command:?})", feature.name());
                            block_on(feature.command(command))?;
                        },
                    );

                    Ok::<(), anyhow::Error>(())
                } => result?,
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
            &std::env::var("CONNECTION_STRING").context("connection string missing")?,
        )
    }
}
