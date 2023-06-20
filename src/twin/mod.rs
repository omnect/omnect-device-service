mod consent;
mod factory_reset;
#[cfg(test)]
#[path = "mod_test.rs"]
mod mod_test;
mod network_status;
mod reboot;
mod ssh;
mod wifi_commissioning;
use crate::twin::{
    consent::DeviceUpdateConsent, factory_reset::FactoryReset, network_status::NetworkStatus,
    reboot::Reboot, ssh::Ssh, wifi_commissioning::WifiCommissioning,
};
use crate::Message;
use anyhow::{bail, Context, Result};
use azure_iot_sdk::client::*;
use dotenvy;
use enum_dispatch::enum_dispatch;
use log::{info, warn};
use once_cell::sync::OnceCell;
use serde_json::json;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{mpsc::Sender, Mutex};
#[cfg(test)]
use strum::EnumCount;
use strum_macros::{EnumCount as EnumCountMacro, EnumIter};

static INSTANCE: OnceCell<Mutex<Option<Twin>>> = OnceCell::new();

pub struct TwinInstance {
    inner: &'static Mutex<Option<Twin>>,
}

pub fn get_or_init(tx: Option<&Sender<Message>>) -> TwinInstance {
    if let Some(tx) = tx {
        TwinInstance {
            inner: INSTANCE.get_or_init(|| Mutex::new(Some(Twin::new(tx.clone())))),
        }
    } else {
        TwinInstance {
            inner: INSTANCE.get().unwrap(),
        }
    }
}

impl TwinInstance {
    pub fn exec<F, T>(&self, f: F) -> Result<T>
    where
        F: Fn(&Twin) -> Result<T>,
    {
        let guard = &*self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let twin = guard.as_ref().unwrap();
        f(twin)
    }

    pub fn exec_mut<F, T>(&self, f: F) -> Result<T>
    where
        F: Fn(&mut Twin) -> Result<T>,
    {
        let guard = &mut *self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let twin = guard.as_mut().unwrap();
        f(twin)
    }

    pub fn direct_methods(&self) -> Option<DirectMethodMap> {
        let mut methods = DirectMethodMap::new();
        methods.insert(
            String::from("factory_reset"),
            IotHubClient::make_direct_method(move |in_json| {
                factory_reset::reset_to_factory_settings(in_json)
            }),
        );
        methods.insert(
            String::from("user_consent"),
            Box::new(consent::user_consent),
        );
        methods.insert(
            String::from("reboot"),
            IotHubClient::make_direct_method(move |_in_json| {
                info!("reboot requested");
                reboot::reboot()?;
                Ok(None)
            }),
        );
        methods.insert(
            String::from("refresh_network_status"),
            IotHubClient::make_direct_method(move |in_json| {
                network_status::refresh_network_status(in_json)
            }),
        );
        methods.insert(
            String::from("refresh_ssh_status"),
            IotHubClient::make_direct_method(ssh::refresh_ssh_status),
        );
        methods.insert(
            String::from("open_ssh"),
            IotHubClient::make_direct_method(ssh::open_ssh),
        );
        methods.insert(
            String::from("close_ssh"),
            IotHubClient::make_direct_method(ssh::close_ssh),
        );

        Some(methods)
    }

    pub fn cloud_message(&self, msg: IotMessage) {
        warn!(
            "received unexpected C2D message with \n body: {:?}\n properties: {:?} \n system properties: {:?}",
            std::str::from_utf8(&msg.body).unwrap(),
            msg.properties,
            msg.system_properties
        );
    }
}

#[derive(Default)]
struct FeatureState {
    tx: Option<Sender<Message>>,
}

#[enum_dispatch]
#[derive(EnumCountMacro, EnumIter)]
enum TwinFeature {
    FactoryReset,
    DeviceUpdateConsent,
    NetworkStatus,
    Ssh,
}

#[enum_dispatch(TwinFeature)]
trait Feature {
    fn name(&self) -> String;

    fn version(&self) -> u8;

    fn is_enabled(&self) -> bool;

    fn state(&self) -> &FeatureState;

    fn state_mut(&mut self) -> &mut FeatureState;

    fn as_any(&self) -> &dyn Any;

    fn report_initial_state(&self) -> Result<()> {
        Ok(())
    }

    fn set_tx(&mut self, tx: Sender<Message>) {
        self.state_mut().tx = Some(tx)
    }

    fn tx(&self) -> &Option<Sender<Message>> {
        &self.state().tx
    }

    fn ensure(&self) -> Result<()> {
        if !self.is_enabled() {
            bail!("feature disabled: {}", self.name());
        }

        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        unimplemented!();
    }

    fn report_availability(&self) -> Result<()> {
        let value = if self.is_enabled() {
            json!({ "version": &self.version() })
        } else {
            json!(null)
        };

        Twin::report_impl(self.tx(), json!({ &self.name(): value }))
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct Twin {
    tx: Option<Sender<Message>>,
    features: HashMap<TypeId, Box<dyn Feature + Send>>,
}

impl Twin {
    pub fn new(tx: Sender<Message>) -> Self {
        Twin {
            tx: Some(tx),
            features: HashMap::from([
                (
                    TypeId::of::<DeviceUpdateConsent>(),
                    Box::new(DeviceUpdateConsent::default()) as Box<dyn Feature + Send>,
                ),
                (
                    TypeId::of::<FactoryReset>(),
                    Box::new(FactoryReset::default()) as Box<dyn Feature + Send>,
                ),
                (
                    TypeId::of::<NetworkStatus>(),
                    Box::new(NetworkStatus::default()) as Box<dyn Feature + Send>,
                ),
                (
                    TypeId::of::<Reboot>(),
                    Box::new(Reboot::default()) as Box<dyn Feature + Send>,
                ),
                (
                    TypeId::of::<Ssh>(),
                    Box::new(Ssh::default()) as Box<dyn Feature + Send>,
                ),
                (
                    TypeId::of::<WifiCommissioning>(),
                    Box::new(WifiCommissioning::default()) as Box<dyn Feature + Send>,
                ),
            ]),
        }
    }

    pub fn init_features(&mut self) -> Result<()> {
        dotenvy::from_path_override(Path::new(&format!(
            "{}/os-release",
            std::env::var("OS_RELEASE_DIR_PATH").unwrap_or_else(|_| "/etc".to_string())
        )))?;

        // report version
        let version = json!({
            "module-version": env!("CARGO_PKG_VERSION"),
            "azure-sdk-version": IotHubClient::sdk_version_string()
        });

        Self::report_impl(&self.tx, version).context("init_features: report_impl")?;

        for f in self.features.values_mut() {
            f.set_tx(
                self.tx
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("tx channel missing"))?
                    .clone(),
            );
        }

        for f in self.features.values_mut() {
            // report feature availability
            f.report_availability()?;
        }

        for f in self.features.values_mut() {
            if f.is_enabled() {
                // report initial feature status
                f.report_initial_state()?;
            }
        }

        for f in self.features.values_mut() {
            if f.is_enabled() {
                // start feature
                f.start()?;
            }
        }

        Ok(())
    }

    pub fn update(&mut self, state: TwinUpdateState, desired: serde_json::Value) -> Result<()> {
        match state {
            TwinUpdateState::Partial => {
                if let Some(gc) = desired.get("general_consent") {
                    let f = self.feature::<DeviceUpdateConsent>()?;

                    if f.is_enabled() {
                        f.update_general_consent(gc.as_array())?;
                    }
                }

                if let Some(inf) = desired.get("include_network_filter") {
                    let f = self.feature_mut::<NetworkStatus>()?;

                    if f.is_enabled() {
                        f.update_include_network_filter(inf.as_array())?;
                    }
                }
            }
            TwinUpdateState::Complete => {
                if desired.get("desired").is_none() {
                    bail!("update: 'desired' missing while TwinUpdateState::Complete")
                }

                self.feature::<DeviceUpdateConsent>()?
                    .update_general_consent(desired["desired"]["general_consent"].as_array())?;

                self.feature_mut::<NetworkStatus>()?
                    .update_include_network_filter(
                        desired["desired"]["include_network_filter"].as_array(),
                    )?;
            }
        }
        Ok(())
    }

    fn feature<T>(&self) -> Result<&T>
    where
        T: Feature + 'static,
    {
        let f = self
            .features
            .get(&TypeId::of::<T>())
            .ok_or_else(|| anyhow::anyhow!("failed to get feature"))?
            .as_any()
            .downcast_ref::<T>()
            .ok_or_else(|| anyhow::anyhow!("failed to cast to feature ref"))?;

        f.ensure()?;

        Ok(f)
    }

    fn feature_mut<T>(&mut self) -> Result<&mut T>
    where
        T: Feature + 'static,
    {
        let f = self
            .features
            .get_mut(&TypeId::of::<T>())
            .ok_or_else(|| anyhow::anyhow!("failed to get feature mut"))?
            .as_any_mut()
            .downcast_mut::<T>()
            .ok_or_else(|| anyhow::anyhow!("failed to cast to feature mut"))?;

        f.ensure()?;

        Ok(f)
    }

    fn report_impl(tx: &Option<Sender<Message>>, value: serde_json::Value) -> Result<()> {
        info!("report: {}", value);

        tx.as_ref()
            .ok_or_else(|| anyhow::anyhow!("tx channel missing"))?
            .send(Message::Reported(value))
            .map_err(|err| err.into())
    }
}
