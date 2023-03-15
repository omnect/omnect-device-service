use super::Twin;
use crate::{twin, ReportProperty};
use anyhow::{Context, Result};
use log::{debug, info};
use serde::Serialize;
use serde_json::json;
use std::io::Write;
use std::process::{Command, Stdio};

static AUTHORIZED_KEY_PATH: &str = "/home/omnect/.ssh/authorized_keys";
static SSH_RULE: &str = "-p tcp -m tcp --dport 22 -m state --state NEW -j ACCEPT";

pub fn write_authorized_keys(pubkey: &str) -> Result<()> {
    let mut child = Command::new("sudo")
        .args(["-u", "omnect", "tee", AUTHORIZED_KEY_PATH])
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

pub fn refresh_ssh_status(_in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("ssh status requested");

    twin::get_or_init(None).report(&ReportProperty::SshStatus)?;

    Ok(None)
}

pub fn open_ssh(in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("open ssh requested {in_json}");

    match in_json["pubkey"].as_str() {
        Some("") => anyhow::bail!("Empty ssh pubkey"),
        Some(pubkey) => write_authorized_keys(pubkey),
        None => anyhow::bail!("No ssh pubkey given"),
    }?;

    let v4 = iptables::new(false).map_err(|e| anyhow::anyhow!("{e}"))?;
    v4.append_replace("filter", "INPUT", SSH_RULE)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let v6 = iptables::new(true).map_err(|e| anyhow::anyhow!("{e}"))?;
    v6.append_replace("filter", "INPUT", SSH_RULE)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    twin::get_or_init(None).report(&ReportProperty::SshStatus)?;

    Ok(None)
}

pub fn close_ssh(_in_json: serde_json::Value) -> Result<Option<serde_json::Value>> {
    info!("close ssh requested");

    let v4 = iptables::new(false).map_err(|e| anyhow::anyhow!("{e}"))?;
    v4.delete("filter", "INPUT", SSH_RULE)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let v6 = iptables::new(true).map_err(|e| anyhow::anyhow!("{e}"))?;
    v6.delete("filter", "INPUT", SSH_RULE)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // clear authorized_keys
    write_authorized_keys("")?;

    twin::get_or_init(None).report(&ReportProperty::SshStatus)?;

    Ok(None)
}

impl Twin {
    pub fn report_ssh_status(&mut self) -> Result<()> {
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
                .exists("filter", "INPUT", SSH_RULE)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            p => anyhow::bail!("Unexpected input policy: {p}"),
        };

        ssh_report.v6_enabled = match v6_input_policy.as_str() {
            "ACCEPT" => true,
            "DROP" => v4
                .exists("filter", "INPUT", SSH_RULE)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            p => anyhow::bail!("Unexpected input policy: {p}"),
        };

        debug!("ssh report: {:#?}", ssh_report);

        self.report_impl(json!({ "ssh_status": json!(ssh_report) }))
            .context("report_ssh_status")
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}
