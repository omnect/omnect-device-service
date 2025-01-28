use super::{
    feature::{Command as FeatureCommand, CommandResult},
    Feature,
};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{debug, error, info, warn};
use serde::{de::Error, Deserialize, Deserializer};
use serde_json::json;
use std::env;
use std::ops::Drop;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc::Sender, OwnedSemaphorePermit, Semaphore, TryAcquireError};
use uuid::Uuid;

static MAX_ACTIVE_TUNNELS: usize = 5;
static SSH_KEY_TYPE: &str = "ed25519";
#[cfg(not(feature = "mock"))]
static SSH_PORT: u16 = 22;
static SSH_TUNNEL_USER: &str = "ssh_tunnel_user";

macro_rules! ssh_tunnel_data {
    () => {{
        static SSH_TUNNEL_DIR_PATH_DEFAULT: &'static str = "/run/omnect-device-service/ssh-tunnel";
        std::env::var("SSH_TUNNEL_DIR_PATH").unwrap_or(SSH_TUNNEL_DIR_PATH_DEFAULT.to_string())
    }};
}

macro_rules! pub_key_path {
    ($name:expr) => {{
        PathBuf::from(&format!(r"{}/{}.pub", ssh_tunnel_data!(), $name))
    }};
}

macro_rules! priv_key_path {
    ($name:expr) => {{
        PathBuf::from(&format!(r"{}/{}", ssh_tunnel_data!(), $name))
    }};
}

macro_rules! control_socket_path {
    ($name:expr) => {{
        PathBuf::from(&format!(r"{}/{}-socket", ssh_tunnel_data!(), $name))
    }};
}

macro_rules! device_cert_file {
    () => {{
        if cfg!(feature = "mock") {
            std::env::var("DEVICE_CERT_FILE").unwrap_or("/mnt/cert/ssh/root_ca".to_string())
        } else {
            "/mnt/cert/ssh/root_ca".to_string()
        }
    }};
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct BastionConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub socket_path: PathBuf,
}

#[cfg(feature = "mock")]
fn exec_as<S: AsRef<str>>(_user: &str, command: S) -> Command {
    Command::new(command.as_ref())
}

#[cfg(not(feature = "mock"))]
fn exec_as<S: AsRef<str>>(user: &str, command: S) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.args(["-u", user]);
    cmd.args([command.as_ref()]);

    cmd
}

fn validate_uuid<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let uuid: String = Deserialize::deserialize(deserializer)?;

    Ok(Uuid::parse_str(&uuid)
        .map_err(D::Error::custom)?
        .as_hyphenated()
        .to_string())
}

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct UpdateDeviceSshCaCommand {
    ssh_tunnel_ca_pub: String,
}

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct GetSshPubKeyCommand {
    #[serde(deserialize_with = "validate_uuid")]
    pub tunnel_id: String,
}

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct OpenSshTunnelCommand {
    #[serde(deserialize_with = "validate_uuid")]
    pub tunnel_id: String,
    pub certificate: String,
    #[serde(flatten)]
    pub bastion_config: BastionConfig,
}

#[derive(Debug, Deserialize, PartialEq)]
pub(crate) struct CloseSshTunnelCommand {
    #[serde(deserialize_with = "validate_uuid")]
    pub tunnel_id: String,
}

pub struct SshTunnel {
    tx_reported_properties: Option<Sender<serde_json::Value>>,
    tx_outgoing_message: Option<Sender<IotMessage>>,
    ssh_tunnel_semaphore: Arc<Semaphore>,
}

#[async_trait(?Send)]
impl Feature for SshTunnel {
    fn name(&self) -> String {
        Self::ID.to_string()
    }

    fn version(&self) -> u8 {
        Self::SSH_TUNNEL_VERSION
    }

    fn is_enabled(&self) -> bool {
        env::var("SUPPRESS_SSH_TUNNEL") != Ok("true".to_string())
    }

    async fn connect_twin(
        &mut self,
        tx_reported_properties: Sender<serde_json::Value>,
        tx_outgoing_message: Sender<IotMessage>,
    ) -> Result<()> {
        self.tx_reported_properties = Some(tx_reported_properties);
        self.tx_outgoing_message = Some(tx_outgoing_message);

        self.report().await
    }

    async fn command(&mut self, cmd: FeatureCommand) -> CommandResult {
        match cmd {
            FeatureCommand::DesiredUpdateDeviceSshCa(cmd) => self.update_device_ssh_ca(cmd).await,
            FeatureCommand::CloseSshTunnel(cmd) => self.close_ssh_tunnel(cmd).await,
            FeatureCommand::GetSshPubKey(cmd) => self.get_ssh_pub_key(cmd).await,
            FeatureCommand::OpenSshTunnel(cmd) => self.open_ssh_tunnel(cmd).await,
            _ => bail!("unexpected command"),
        }
    }
}

impl SshTunnel {
    const SSH_TUNNEL_VERSION: u8 = 2;
    const ID: &'static str = "ssh_tunnel";

    pub fn new() -> Self {
        SshTunnel {
            tx_reported_properties: None,
            tx_outgoing_message: None,
            ssh_tunnel_semaphore: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
        }
    }

    async fn update_device_ssh_ca(&self, args: UpdateDeviceSshCaCommand) -> CommandResult {
        info!("update device ssh cert requested");

        let mut child = exec_as(SSH_TUNNEL_USER, "tee")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .arg(device_cert_file!())
            .spawn()
            .context("failed to spawn update ssh ca pub key command")?;

        let data = args.ssh_tunnel_ca_pub.as_bytes();

        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(data).await?;
        drop(stdin); // necessary to close stdin

        if !child.wait().await?.success() {
            bail!("failed to update ssh ca pub key");
        }

        self.report().await?;

        Ok(None)
    }

    async fn get_ssh_pub_key(&self, args: GetSshPubKeyCommand) -> CommandResult {
        info!("ssh pub key requested");

        let (priv_key_path, pub_key_path) = (
            priv_key_path!(args.tunnel_id),
            pub_key_path!(args.tunnel_id),
        );
        Self::create_key_pair(priv_key_path)
            .await
            .context("get_ssh_pub_key: {e}")?;

        Ok(Some(
            json!({ "key": Self::get_pub_key(&pub_key_path).await? }),
        ))
    }

    async fn create_key_pair(priv_key_path: PathBuf) -> Result<()> {
        let mut child = Self::create_key_pair_command(priv_key_path)?;

        // In principle we should not get key file conflicts. However, in case
        // we do get conflicts, ssh-keygen will hang indefinitely. We therefore
        // tell it to overwrite any existing keys. Removing the key beforehand
        // could lead to TOCTOU bugs.
        let Some(mut stdin) = child.stdin.take() else {
            bail!("create_key_pair: failed to get stdin")
        };
        stdin.write_all("y\n".as_bytes()).await?;

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            bail!(
                "create_key_pair: failed to create ssh key pair: {}",
                str::from_utf8(&output.stderr)?
            );
        }

        Ok(())
    }

    fn create_key_pair_command(priv_key_path: PathBuf) -> Result<Child> {
        exec_as(SSH_TUNNEL_USER, "ssh-keygen")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(["-q"])
            .args(["-f", priv_key_path.to_string_lossy().as_ref()])
            .args(["-t", SSH_KEY_TYPE])
            .args(["-N", ""])
            .spawn()
            .context("Failed to create ssh key pair")
    }

    async fn get_pub_key(pub_key_path: &Path) -> Result<String> {
        let child = exec_as(SSH_TUNNEL_USER, "cat")
            .stdout(Stdio::piped())
            .arg(pub_key_path.to_string_lossy().as_ref())
            .spawn()
            .context("Failed to get pub key")?;

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            bail!("Failed to retrieve ssh public key.");
        }

        Ok(str::from_utf8(&output.stdout)?.to_string())
    }

    async fn open_ssh_tunnel(&self, args: OpenSshTunnelCommand) -> CommandResult {
        info!("open ssh tunnel requested");

        let ssh_tunnel_permit = match self.ssh_tunnel_semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                bail!("ssh_tunnel: maximum number of active ssh connections is reached")
            }
            Err(_other) => bail!("ssh_tunnel: failed to lock tunnel"),
        };

        // ensure our ssh keys and certificate are cleaned up properly when
        // leaving this function
        let ssh_creds = SshCredentialsGuard::new(&priv_key_path!(&args.tunnel_id))
            .context("Failed to open ssh tunnel")?;

        // store the certificate so that ssh can use it for login on the bastion host
        store_ssh_cert(&ssh_creds.cert(), &args.certificate).await?;

        let mut ssh_process =
            Self::start_tunnel_command(&args.tunnel_id, &ssh_creds, &args.bastion_config)?;

        let Some(stdout) = ssh_process.stdout.take() else {
            bail!("open_ssh_tunnel: stdout is None");
        };

        let Some(tx) = &self.tx_outgoing_message else {
            bail!("open_ssh_tunnel: tx_outgoing_message is None")
        };

        // report tunnel termination once it completes
        tokio::spawn(Self::await_tunnel_termination(
            ssh_process,
            args.tunnel_id.clone(),
            tx.clone(),
            ssh_tunnel_permit,
            ssh_creds,
        ));

        Self::await_tunnel_creation(stdout).await?;

        debug!(
            "Successfully established connection \"{}\" to \"{}:{}\"",
            args.tunnel_id, args.bastion_config.host, args.bastion_config.port
        );

        Ok(None)
    }

    #[cfg(not(feature = "mock"))]
    fn start_tunnel_command(
        tunnel_id: &str,
        ssh_creds: &SshCredentialsGuard,
        bastion_config: &BastionConfig,
    ) -> Result<Child> {
        debug!(
            "Starting ssh tunnel \"{}\" bastion host: \"{}:{}\", bastion user: \"{}\"",
            tunnel_id, bastion_config.host, bastion_config.port, bastion_config.user
        );

        exec_as(SSH_TUNNEL_USER, "ssh")
            // closing stdin is functionally not necessary but fixes issues with logging
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(["-M"])
            .args([
                "-S",
                control_socket_path!(&tunnel_id).to_string_lossy().as_ref(),
            ]) // path to the local control socket
            .args(["-i", ssh_creds.priv_key().to_string_lossy().as_ref()])
            .args([
                "-o",
                &format!("CertificateFile={}", ssh_creds.cert().to_string_lossy()),
            ]) // path to the bastion host certificate
            .args([
                "-R",
                &format!(
                    "{}/{}:localhost:{}",
                    bastion_config.socket_path.to_string_lossy(),
                    tunnel_id,
                    SSH_PORT
                ),
            ]) // create a reverse proxy on bastion host as a unix socket at `socket_path`
            .args([&format!("{}@{}", bastion_config.user, bastion_config.host)])
            .args(["-p", &format!("{}", bastion_config.port)])
            .args(["-o", "PreferredAuthentications=publickey"]) // enforce cert-based authentication
            .args(["-o", "ExitOnForwardFailure=yes"]) // ensure ssh terminates if anything goes south
            .args(["-o", "StrictHostKeyChecking=no"]) // allow bastion host to be redeployed
            .args(["-o", "UserKnownHostsFile=/dev/null"])
            .spawn()
            .context("open_ssh_tunnel: failed setting up tunnel to bastion host")
    }

    #[cfg(feature = "mock")]
    fn start_tunnel_command(
        _tunnel_id: &str,
        _ssh_creds: &SshCredentialsGuard,
        bastion_config: &BastionConfig,
    ) -> Result<Child> {
        Command::new("bash")
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            // hacky workaround to get the named pipe path. With the named
            // pipe we can block the mock process externally
            .args([
                "-c",
                &format!("echo established && cat < {} || true", &bastion_config.host),
            ])
            .spawn()
            .context("start_tunnel_command: failed")
    }

    async fn await_tunnel_termination(
        ssh_process: Child,
        tunnel_id: String,
        tx_outgoing_message: Sender<IotMessage>,
        _ssh_tunnel_permit: OwnedSemaphorePermit, // take ownership of the permit to drop semaphore once channel closes
        _ssh_creds: SshCredentialsGuard,
    ) {
        let output = match ssh_process.wait_with_output().await {
            Ok(output) => output,
            Err(err) => {
                error!("Could not retrieve output from ssh process: {}", err);
                return;
            }
        };

        if !output.status.success() {
            warn!(
                "SSH command exited with errors: {}",
                str::from_utf8(&output.stderr).unwrap()
            );
        }

        let result = notify_tunnel_termination(tx_outgoing_message, &tunnel_id, None).await;

        if let Err(err) = result {
            warn!("Failed to send tunnel update to cloud: {err}");
        }

        info!("Closed ssh tunnel: {}", tunnel_id);
    }

    async fn await_tunnel_creation(stdout: tokio::process::ChildStdout) -> Result<()> {
        let mut reader = BufReader::new(stdout).lines();
        let response = reader.next_line().await;

        match response {
            Ok(Some(msg)) => {
                // the ssh certificate is configured such that it executes an
                // "echo established" upon successful connection. We use this to
                // detect a successfully established ssh tunnel
                if msg == "established" {
                    Ok(())
                } else {
                    bail!("open_ssh_tunnel: Failed to establish ssh tunnel due to unexpected response from ssh server: {}", msg);
                }
            }
            Ok(None) => {
                bail!("open_ssh_tunnel: Failed to establish ssh tunnel");
            }
            Err(err) => {
                bail!("open_ssh_tunnel: Failed to establish ssh tunnel since unable to read from ssh process: {}", err);
            }
        }
    }

    async fn close_ssh_tunnel(&self, args: CloseSshTunnelCommand) -> CommandResult {
        info!("close ssh tunnel requested");

        let control_socket_path = control_socket_path!(&args.tunnel_id);

        debug!(
            "Closing ssh tunnel \"{}\", socket path: \"{}\"",
            args.tunnel_id,
            control_socket_path.display()
        );

        Self::close_tunnel_command(&control_socket_path).await?;

        Ok(None)
    }

    async fn close_tunnel_command(control_socket_path: &Path) -> Result<()> {
        let result = exec_as(SSH_TUNNEL_USER, "ssh")
            .stdout(Stdio::piped())
            .args(["-O", "exit"])
            .args(["-S", control_socket_path.to_string_lossy().as_ref()])
            .args(["bastion_host"]) // the destination host name is not used but necessary for the ssh command here
            .output()
            .await
            .context("close_ssh_tunnel: failed")?;

        if !result.status.success() {
            warn!(
                "Unexpected error upon closing tunnel: {}",
                str::from_utf8(&result.stderr).unwrap()
            );
        }

        Ok(())
    }

    async fn report(&self) -> Result<()> {
        let Some(tx) = &self.tx_reported_properties else {
            warn!("report: skip since tx_reported_properties is None");
            return Ok(());
        };

        let mut response = json!({
            "ssh_tunnel": {
                "version": self.version(),
            }
        });

        let device_ca_path = device_cert_file!();

        if let Ok(ca_data) = std::fs::read_to_string(&device_ca_path) {
            response["ssh_tunnel"]["ca_pub"] = json!(ca_data.trim_end().to_string());
        } else {
            warn!("unable to read ssh public ca data");

            // we signal the backend that we don't have a pub ca set.
            response["ssh_tunnel"]["ca_pub"] = json!(null);
        };

        tx.send(response).await.context("report: send")
    }
}

async fn store_ssh_cert(cert_path: &Path, data: &str) -> Result<()> {
    let mut child = exec_as(SSH_TUNNEL_USER, "tee")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg(&*cert_path.to_string_lossy())
        .spawn()
        .context("store_ssh_cert: failed to spawn command")?;

    let Some(mut stdin) = child.stdin.take() else {
        bail!("store_ssh_cert: failed to get stdin")
    };
    stdin
        .write_all(data.as_bytes())
        .await
        .context("store_ssh_cert: failed to write to stdin")?;
    drop(stdin); // necessary to close stdin

    if !child
        .wait()
        .await
        .context("store_ssh_cert: failed to wait for command")?
        .success()
    {
        bail!("store_ssh_cert: command failed");
    }

    Ok(())
}

// RAII handle to the temporary bastion host certificate
struct SshCredentialsGuard {
    key_path: PathBuf,
}

impl SshCredentialsGuard {
    fn new(key_path: &Path) -> Result<SshCredentialsGuard> {
        ensure!(key_path.parent().is_some());

        Ok(SshCredentialsGuard {
            key_path: key_path.into(),
        })
    }

    fn pub_key(&self) -> PathBuf {
        self.key_path.with_extension("pub")
    }

    fn priv_key(&self) -> PathBuf {
        self.key_path.clone()
    }

    fn cert(&self) -> PathBuf {
        let file_name = self.key_path.file_name().unwrap(); // ensured by new
        let cert_name = format!("{}-cert.pub", file_name.to_string_lossy());
        self.key_path.with_file_name(cert_name)
    }
}

fn remove_file(file: &PathBuf) -> Result<()> {
    if cfg!(feature = "mock") {
        std::fs::remove_file(file)?;
    } else {
        std::process::Command::new("sudo")
            .args(["-u", SSH_TUNNEL_USER])
            .args(["rm", &*file.to_string_lossy()])
            .output()?;
    };

    Ok(())
}

impl Drop for SshCredentialsGuard {
    fn drop(&mut self) {
        [self.pub_key(), self.priv_key(), self.cert()]
            .iter()
            .for_each(|file| {
                if let Err(err) = remove_file(file) {
                    warn!(
                        "Failed to delete certificate \"{}\": {}",
                        file.to_string_lossy(),
                        err
                    );
                }
            })
    }
}

async fn notify_tunnel_termination(
    tx_outgoing_message: Sender<IotMessage>,
    tunnel_id: &str,
    error: Option<&str>,
) -> Result<()> {
    let msg = IotMessage::builder()
        .set_body(
            serde_json::to_vec(&serde_json::json!({
                "tunnel_id": tunnel_id,
                "error": error.unwrap_or("none"),
            }))
            .context("notify_tunnel_termination: build body")?,
        )
        .set_content_type("application/json")
        .set_content_encoding("utf-8")
        .build()
        .context("notify_tunnel_termination: build message")?;

    tx_outgoing_message
        .send(msg)
        .await
        .context("notify_tunnel_termination: send message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;
    use regex::Regex;
    use std::fs::File;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn validate_correct_uuid() {
        let uuid = "c0c7d30c-511a-4fed-80b2-b4cccc74228e";
        let uuid_message = format!("{{\"tunnel_id\": \"{uuid}\"}}");

        let args = serde_json::from_str::<GetSshPubKeyCommand>(&uuid_message).unwrap();

        assert_eq!(args.tunnel_id, uuid);
    }

    #[test]
    #[should_panic]
    fn validate_incorrect_uuid() {
        let uuid = "!@#$S";
        let uuid_message = format!("{{\"tunnel_id\": \"{uuid}\"}}");

        let _args = serde_json::from_str::<GetSshPubKeyCommand>(&uuid_message).unwrap();
    }

    #[test]
    fn certificate_file_raii() {
        let key_dir = tempdir().unwrap();

        const KEY_NAME: &str = "key";

        let priv_key_path = key_dir.path().join(KEY_NAME);
        let pub_key_path = key_dir.path().join(format!("{}.pub", KEY_NAME));
        let cert_path = key_dir.path().join(format!("{}-cert.pub", KEY_NAME));

        let _priv_key_file = File::create(&priv_key_path).unwrap();
        let _pub_key_file = File::create(&pub_key_path).unwrap();
        let _cert_file = File::create(&cert_path).unwrap();

        assert!(priv_key_path.exists());
        assert!(pub_key_path.exists());
        assert!(cert_path.exists());

        {
            let _file = SshCredentialsGuard::new(&priv_key_path).unwrap();
        }

        assert!(!priv_key_path.exists());
        assert!(!pub_key_path.exists());
        assert!(!cert_path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reported_property_no_ca_file_available() {
        let (tx_outgoing_message, _rx_outgoing_message) = tokio::sync::mpsc::channel(100);
        let (tx_reported_properties, mut rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let ssh_tunnel = SshTunnel {
            tx_outgoing_message: Some(tx_outgoing_message),
            tx_reported_properties: Some(tx_reported_properties),
            ssh_tunnel_semaphore: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
        };
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp_file = tmp_dir.path().join("some-ca-file");
        env::set_var("DEVICE_CERT_FILE", &tmp_file);

        let _response = block_on(async { ssh_tunnel.report().await }).unwrap();

        let reported_properties = rx_reported_properties.try_recv().unwrap();

        assert_eq!(
            reported_properties,
            json!({
                "ssh_tunnel": {
                    "version": 2,
                    "ca_pub": null,
                }
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reported_property_trailing_whitespaces_and_newlines_are_stripped_from_ca_file() {
        const CERTIFICATE_DATA: &str = "Some Device Certificate Content";

        let (tx_outgoing_message, _rx_outgoing_message) = tokio::sync::mpsc::channel(100);
        let (tx_reported_properties, mut rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let ssh_tunnel = SshTunnel {
            tx_outgoing_message: Some(tx_outgoing_message),
            tx_reported_properties: Some(tx_reported_properties),
            ssh_tunnel_semaphore: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
        };
        let tmp_file = tempfile::NamedTempFile::new().unwrap();
        env::set_var("DEVICE_CERT_FILE", &tmp_file.path());

        std::fs::write(&tmp_file, format!("{CERTIFICATE_DATA}\n")).unwrap();

        let _response = block_on(async { ssh_tunnel.report().await }).unwrap();

        let reported_properties = rx_reported_properties.try_recv().unwrap();

        assert_eq!(
            reported_properties,
            json!({
                "ssh_tunnel": {
                    "version": 2,
                    "ca_pub": CERTIFICATE_DATA,
                }
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_device_ssh_ca_should_update_ca_file() {
        const CERTIFICATE_DATA: &str = "Some Device Certificate Content";

        let (tx_outgoing_message, _rx_outgoing_message) = tokio::sync::mpsc::channel(100);
        let (tx_reported_properties, mut rx_reported_properties) = tokio::sync::mpsc::channel(100);
        let mut ssh_tunnel = SshTunnel {
            tx_outgoing_message: Some(tx_outgoing_message),
            tx_reported_properties: Some(tx_reported_properties),
            ssh_tunnel_semaphore: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
        };
        let tmp_file = tempfile::NamedTempFile::new().unwrap();
        env::set_var("DEVICE_CERT_FILE", &tmp_file.path());

        let _response = block_on(async {
            ssh_tunnel
                .command(FeatureCommand::DesiredUpdateDeviceSshCa(
                    UpdateDeviceSshCaCommand {
                        ssh_tunnel_ca_pub: CERTIFICATE_DATA.to_string(),
                    },
                ))
                .await
        })
        .unwrap();

        let result = std::fs::read_to_string(&tmp_file.path()).unwrap();

        assert_eq!(CERTIFICATE_DATA, result);

        let reported_properties = rx_reported_properties.try_recv().unwrap();

        assert_eq!(
            reported_properties,
            json!({
                "ssh_tunnel": {
                    "version": 2,
                    "ca_pub": CERTIFICATE_DATA,
                }
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_ssh_pub_key_test() {
        let (tx_outgoing_message, _rx_outgoing_message) = tokio::sync::mpsc::channel(100);
        let mut ssh_tunnel = SshTunnel {
            tx_outgoing_message: Some(tx_outgoing_message),
            ssh_tunnel_semaphore: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
        };
        let tmp_dir = tempfile::tempdir().unwrap();
        env::set_var("SSH_TUNNEL_DIR_PATH", tmp_dir.path());

        // test creation of pub key
        let tunnel_id = "b054a76d-520c-40a9-b401-0f6bfb7cee9b".to_string();
        let pub_key_regex = Regex::new(
            r#"^\{"key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5[0-9A-Za-z+/]+[=]{0,3}(\s.*)\\n"\}?$"#,
        )
        .unwrap();

        let response = block_on(async {
            ssh_tunnel
                .command(FeatureCommand::GetSshPubKey(GetSshPubKeyCommand {
                    tunnel_id,
                }))
                .await
        })
        .unwrap()
        .unwrap()
        .to_string();

        assert!(pub_key_regex.is_match(&response));

        // test for correct handling of existing private key file
        std::fs::copy(
            "testfiles/positive/b7afb216-5f7a-4755-a300-9374f8a0e9ff",
            tmp_dir.path().join("b7afb216-5f7a-4755-a300-9374f8a0e9ff"),
        )
        .unwrap();
        let tunnel_id = "b7afb216-5f7a-4755-a300-9374f8a0e9ff".to_string();

        let response = block_on(async {
            ssh_tunnel
                .command(FeatureCommand::GetSshPubKey(GetSshPubKeyCommand {
                    tunnel_id,
                }))
                .await
        })
        .unwrap()
        .unwrap()
        .to_string();

        assert!(!response.starts_with(
            "-----BEGIN OPENSSH PRIVATE KEY-----
        b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
        "
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_ssh_tunnel_test() {
        let (tx_outgoing_message, _rx_outgoing_message) = tokio::sync::mpsc::channel(100);
        let mut ssh_tunnel = SshTunnel {
            tx_outgoing_message: Some(tx_outgoing_message),
            ssh_tunnel_semaphore: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
        };
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("cert.pub");
        std::fs::copy("testfiles/positive/cert.pub", cert_path.clone()).unwrap();
        env::set_var("SSH_TUNNEL_DIR_PATH", tmp_dir.path());

        // test successful
        assert!(block_on(async {
            ssh_tunnel
                .command(FeatureCommand::OpenSshTunnel(OpenSshTunnelCommand {
                    tunnel_id: "b7afb216-5f7a-4755-a300-9374f8a0e9ff".to_string(),
                    certificate: std::fs::read_to_string(cert_path.clone()).unwrap(),
                    bastion_config: BastionConfig {
                        host: "test-host".to_string(),
                        port: 2222,
                        user: "test-user".to_string(),
                        socket_path: std::path::PathBuf::from_str("/some/test/socket/path")
                            .unwrap(),
                    },
                }))
                .await
        })
        .is_ok());

        // test connection limit
        let pipe_names = (1..=5)
            .into_iter()
            .map(|pipe_num| tmp_dir.path().join(&format!("named_pipe_{}", pipe_num)))
            .collect::<Vec<_>>();

        for pipe_name in &pipe_names {
            Command::new("mkfifo")
                .arg(pipe_name)
                .output()
                .await
                .unwrap();
        }

        // the first 5 requests should succeed
        for pipe_name in &pipe_names[0..=4] {
            assert!(block_on(async {
                ssh_tunnel
                    .command(FeatureCommand::OpenSshTunnel(OpenSshTunnelCommand {
                        tunnel_id: "b7afb216-5f7a-4755-a300-9374f8a0e9ff".to_string(),
                        certificate: std::fs::read_to_string(cert_path.clone()).unwrap(),
                        bastion_config: BastionConfig {
                            host: pipe_name.to_str().unwrap().to_string(),
                            port: 2222,
                            user: "test-user".to_string(),
                            socket_path: PathBuf::from_str("/some/test/socket/path").unwrap(),
                        },
                    }))
                    .await
            })
            .is_ok());
        }

        // the final should fail
        assert!(block_on(async {
            ssh_tunnel
                .command(FeatureCommand::OpenSshTunnel(OpenSshTunnelCommand {
                    tunnel_id: "b7afb216-5f7a-4755-a300-9374f8a0e9ff".to_string(),
                    certificate: std::fs::read_to_string(cert_path.clone()).unwrap(),
                    bastion_config: BastionConfig {
                        host: "test-host".to_string(),
                        port: 2222,
                        user: "test-user".to_string(),
                        socket_path: PathBuf::from_str("/some/test/socket/path").unwrap(),
                    },
                }))
                .await
        })
        .is_err());

        // finally, close the pipes. By opening and closing for writing is
        // sufficient.
        for pipe_name in pipe_names {
            let pipe_file = std::fs::File::options()
                .write(true)
                .open(pipe_name)
                .unwrap();
            drop(pipe_file);
        }
        info!("done with all files");
        // we can't wait for the spawned completion tasks here
    }
}
