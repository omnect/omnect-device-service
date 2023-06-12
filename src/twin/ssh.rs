use super::{Feature, FeatureState};
use crate::twin;
use crate::twin::Twin;
use anyhow::{Context, Result};
#[cfg(not(test))]
use log::debug;
use log::info;
use serde::Serialize;
use serde_json::json;
use std::any::Any;
use std::env;
#[cfg(not(test))]
use std::io::Write;
#[cfg(not(test))]
use std::process::{Command, Stdio};

pub fn refresh_ssh_status(_in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    twin::get_or_init(None).exec(|twin| twin.feature::<Ssh>()?.refresh_ssh_status())
}

pub fn open_ssh(in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    twin::get_or_init(None).exec(|twin| twin.feature::<Ssh>()?.open_ssh(in_json.to_owned()))
}

pub fn close_ssh(_in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    twin::get_or_init(None).exec(|twin| twin.feature::<Ssh>()?.close_ssh())
}

#[derive(Default)]
pub struct Ssh {
    state: FeatureState,
}

impl Feature for Ssh {
    fn name(&self) -> String {
        Ssh::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::SSH_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_SSH_HANDLING") != Ok("true".to_string())
    }

    fn report_initial_state(&self) -> Result<()> {
        self.report_ssh_status()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn state_mut(&mut self) -> &mut FeatureState {
        &mut self.state
    }

    fn state(&self) -> &FeatureState {
        &self.state
    }
}

impl Ssh {
    const SSH_VERSION: u8 = 1;
    const ID: &'static str = "ssh";
    const SSH_RULE: &'static str = "-p tcp -m tcp --dport 22 -m state --state NEW -j ACCEPT";
    #[cfg(not(test))]
    const AUTHORIZED_KEYS_PATH: &'static str = "/home/omnect/.ssh/authorized_keys";

    #[cfg(not(test))]
    fn write_authorized_keys(pubkey: &str) -> Result<()> {
        let mut child = Command::new("sudo")
            .args(["-u", "omnect", "tee", Self::AUTHORIZED_KEYS_PATH])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;

        match child.stdin.as_mut() {
            Some(child_stdin) => child_stdin.write_all(format!("{}\n", pubkey).as_bytes())?,
            _ => anyhow::bail!("failed to take stdin"),
        }

        anyhow::ensure!(
            child.wait_with_output()?.status.success(),
            "failed to set pubkey as omnect user"
        );

        Ok(())
    }

    #[cfg(test)]
    fn write_authorized_keys(_pubkey: &str) -> Result<()> {
        Ok(())
    }

    pub fn refresh_ssh_status(&self) -> Result<Option<serde_json::Value>> {
        info!("ssh status requested");

        self.ensure()?;

        self.report_ssh_status()?;

        Ok(None)
    }

    pub fn open_ssh(&self, in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
        info!("open ssh requested {in_json}");

        self.ensure()?;

        match in_json["pubkey"].as_str() {
            Some("") => anyhow::bail!("Empty ssh pubkey"),
            Some(pubkey) => Self::write_authorized_keys(pubkey),
            None => anyhow::bail!("No ssh pubkey given"),
        }?;

        let v4 = iptables::new(false).map_err(|e| anyhow::anyhow!("{e}"))?;
        v4.append_replace("filter", "INPUT", Self::SSH_RULE)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let v6 = iptables::new(true).map_err(|e| anyhow::anyhow!("{e}"))?;
        v6.append_replace("filter", "INPUT", Self::SSH_RULE)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        self.report_ssh_status()?;

        Ok(None)
    }

    pub fn close_ssh(&self) -> Result<Option<serde_json::Value>> {
        info!("close ssh requested");

        self.ensure()?;

        let v4 = iptables::new(false).map_err(|e| anyhow::anyhow!("{e}"))?;
        v4.delete("filter", "INPUT", Self::SSH_RULE)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let v6 = iptables::new(true).map_err(|e| anyhow::anyhow!("{e}"))?;
        v6.delete("filter", "INPUT", Self::SSH_RULE)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // clear authorized_keys
        Self::write_authorized_keys("")?;

        self.report_ssh_status()?;

        Ok(None)
    }

    #[cfg(not(test))]
    fn report_ssh_status(&self) -> Result<()> {
        self.ensure()?;

        #[derive(Debug, Serialize)]
        struct SshReport {
            #[serde(default)]
            v4_enabled: bool,
            v6_enabled: bool,
        }

        let v4 = iptables::new(false).map_err(|e| anyhow::anyhow!("{e}"))?;
        let v6 = iptables::new(true).map_err(|e| anyhow::anyhow!("{e}"))?;

        let v4_input_policy = v4
            .get_policy("filter", "INPUT")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let v6_input_policy = v6
            .get_policy("filter", "INPUT")
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut ssh_report = SshReport {
            v4_enabled: false,
            v6_enabled: false,
        };

        ssh_report.v4_enabled = match v4_input_policy.as_str() {
            "ACCEPT" => true,
            "DROP" => v4
                .exists("filter", "INPUT", Self::SSH_RULE)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            p => anyhow::bail!("Unexpected input policy: {p}"),
        };

        ssh_report.v6_enabled = match v6_input_policy.as_str() {
            "ACCEPT" => true,
            "DROP" => v4
                .exists("filter", "INPUT", Self::SSH_RULE)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            p => anyhow::bail!("Unexpected input policy: {p}"),
        };

        debug!("ssh report: {:#?}", ssh_report);

        Twin::report_impl(
            self.tx(),
            json!({
                    "ssh": {
                        "status": json!(ssh_report)
                    }
            }),
        )
        .context("report_ssh_status")
        .map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[cfg(test)]
    fn report_ssh_status(&self) -> Result<()> {
        #[derive(Debug, Serialize)]
        struct SshReport {
            #[serde(default)]
            v4_enabled: bool,
            v6_enabled: bool,
        }

        let ssh_report = SshReport {
            v4_enabled: false,
            v6_enabled: false,
        };

        Twin::report_impl(
            self.tx(),
            json!({
                    "ssh": {
                        "status": json!(ssh_report)
                    }
            }),
        )
        .context("report_ssh_status")
        .map_err(|e| anyhow::anyhow!("{e}"))
    }
}
