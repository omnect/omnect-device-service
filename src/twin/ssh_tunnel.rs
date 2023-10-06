use super::Feature;
use anyhow::{Context, Result};
use async_trait::async_trait;
use azure_iot_sdk::client::IotMessage;
use log::{error, info, warn};
use serde::{de::Error, Deserialize, Deserializer};
use serde_json::json;
use std::any::Any;
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
#[cfg(not(feature = "mock"))]
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

#[allow(dead_code)]
#[derive(Deserialize)]
struct BastionConfig {
    host: String,
    port: u16,
    user: String,
    socket_path: PathBuf,
}

#[cfg(feature = "mock")]
fn exec_as_tunnel_user<S: AsRef<str>>(command: S) -> Command {
    Command::new(command.as_ref())
}

#[cfg(not(feature = "mock"))]
fn exec_as_tunnel_user<S: AsRef<str>>(command: S) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.args(["-u", SSH_TUNNEL_USER]);
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

#[derive(Deserialize)]
struct GetSshPubKeyArgs {
    #[serde(deserialize_with = "validate_uuid")]
    tunnel_id: String,
}

#[derive(Deserialize)]
struct OpenSshTunnelArgs {
    #[serde(deserialize_with = "validate_uuid")]
    tunnel_id: String,
    certificate: String,
    #[serde(flatten)]
    bastion_config: BastionConfig,
}

#[derive(Deserialize)]
struct CloseSshTunnelArgs {
    #[serde(deserialize_with = "validate_uuid")]
    tunnel_id: String,
}

macro_rules! unpack_args {
    ($func:literal, $args:expr) => {
        serde_json::from_value($args).map_err(|e| anyhow::anyhow!("{}: {e}", $func))
    };
}

pub struct SshTunnel {
    tx_outgoing_message: Sender<IotMessage>,
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

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }
}

impl SshTunnel {
    const SSH_TUNNEL_VERSION: u8 = 1;
    const ID: &'static str = "ssh_tunnel";

    pub fn new(tx_outgoing_message: Sender<IotMessage>) -> Self {
        SshTunnel {
            tx_outgoing_message,
            ssh_tunnel_semaphore: Arc::new(Semaphore::new(MAX_ACTIVE_TUNNELS)),
        }
    }

    pub async fn get_ssh_pub_key(
        &self,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("ssh pub key requested");

        self.ensure()?;

        let args: GetSshPubKeyArgs = unpack_args!("get_ssh_pub_key", args)?;

        let (priv_key_path, pub_key_path) = (
            priv_key_path!(args.tunnel_id),
            pub_key_path!(args.tunnel_id),
        );
        Self::create_key_pair(priv_key_path)
            .await
            .map_err(|e| anyhow::anyhow!("get_ssh_pub_key: {e}"))?;

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
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all("y\n".as_bytes()).await?;

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            error!(
                "Failed to create ssh key pair: {}",
                str::from_utf8(&output.stderr)?
            );
            anyhow::bail!("Error on ssh key creation.");
        }

        Ok(())
    }

    fn create_key_pair_command(priv_key_path: PathBuf) -> Result<Child> {
        exec_as_tunnel_user("ssh-keygen")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(["-q"])
            .args(["-f", priv_key_path.to_string_lossy().as_ref()])
            .args(["-t", SSH_KEY_TYPE])
            .args(["-N", ""])
            .spawn()
            .map_err(|err| {
                error!("Failed to create ssh key pair: {}", err);
                anyhow::anyhow!("Error on ssh key creation.")
            })
    }

    async fn get_pub_key(pub_key_path: &Path) -> Result<String> {
        let child = exec_as_tunnel_user("cat")
            .stdout(Stdio::piped())
            .arg(pub_key_path.to_string_lossy().as_ref())
            .spawn()
            .map_err(|err| anyhow::anyhow!("get_ssh_pub_key: {err}"))?;

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            anyhow::bail!("Failed to retrieve ssh public key.");
        }

        Ok(str::from_utf8(&output.stdout)?.to_string())
    }

    pub async fn open_ssh_tunnel(
        &self,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("open ssh tunnel requested");

        self.ensure()?;

        let args: OpenSshTunnelArgs = unpack_args!("open_ssh_tunnel", args)?;

        let ssh_tunnel_permit = match self.ssh_tunnel_semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                anyhow::bail!("ssh_tunnel: maximum number of active ssh connections is reached")
            }
            Err(_other) => anyhow::bail!("ssh_tunnel: failed to lock tunnel"),
        };

        // ensure our ssh keys and certificate are cleaned up properly when
        // leaving this function
        let ssh_creds = SshCredentialsGuard::new(&priv_key_path!(&args.tunnel_id))
            .map_err(|e| anyhow::anyhow!("open_ssh_tunnel: {e}"))?;

        // store the certificate so that ssh can use it for login on the bastion host
        store_ssh_cert(&ssh_creds.cert(), &args.certificate).await?;

        let mut ssh_process =
            Self::start_tunnel_command(&args.tunnel_id, &ssh_creds, &args.bastion_config)?;
        let stdout = ssh_process.stdout.take().unwrap();

        // report tunnel termination once it completes
        tokio::spawn(Self::await_tunnel_termination(
            ssh_process,
            args.tunnel_id.clone(),
            self.tx_outgoing_message.clone(),
            ssh_tunnel_permit,
        ));

        if let Err(err) = Self::await_tunnel_creation(stdout).await {
            Err(err)?
        }

        log::debug!(
            "Successfully established connection \"{}\" to \"{}:{}\"",
            args.tunnel_id,
            args.bastion_config.host,
            args.bastion_config.port
        );

        Ok(None)
    }

    #[cfg(not(feature = "mock"))]
    fn start_tunnel_command(
        tunnel_id: &str,
        ssh_creds: &SshCredentialsGuard,
        bastion_config: &BastionConfig,
    ) -> Result<Child> {
        log::debug!(
            "Starting ssh tunnel \"{}\" bastion host: \"{}:{}\", bastion user: \"{}\"",
            tunnel_id,
            bastion_config.host,
            bastion_config.port,
            bastion_config.user
        );

        exec_as_tunnel_user("ssh")
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
            .args(["-o", "ExitOnForwardFailure=yes"]) // ensure ssh terminates if anything goes south
            .args(["-o", "StrictHostKeyChecking=no"]) // allow bastion host to be redeployed
            .args(["-o", "UserKnownHostsFile=/dev/null"])
            .spawn()
            .map_err(|e| anyhow::anyhow!("open_ssh_tunnel: {e}"))
    }

    #[cfg(feature = "mock")]
    fn start_tunnel_command(
        _tunnel_id: &str,
        _ssh_creds: &SshCredentialsGuard,
        bastion_config: &BastionConfig,
    ) -> Result<Child> {
        Ok(Command::new("bash")
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            // hacky workaround to get the named pipe path. With the named
            // pipe we can block the mock process externally
            .args([
                "-c",
                &format!("echo established && cat < {} || true", &bastion_config.host),
            ])
            .spawn()
            .unwrap())
    }

    async fn await_tunnel_termination(
        ssh_process: Child,
        tunnel_id: String,
        tx_outgoing_message: Sender<IotMessage>,
        _ssh_tunnel_permit: OwnedSemaphorePermit, // take ownership of the permit to drop semaphore once channel closes
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
                    warn!("Got unexpected response from ssh server: {}", msg);
                    anyhow::bail!("open_ssh_tunnel: Failed to establish ssh tunnel");
                }
            }
            Ok(None) => {
                anyhow::bail!("open_ssh_tunnel: Failed to establish ssh tunnel");
            }
            Err(err) => {
                error!("Could not read from ssh process: {}", err);
                anyhow::bail!("open_ssh_tunnel: Failed to establish ssh tunnel");
            }
        }
    }

    pub async fn close_ssh_tunnel(
        &self,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("close ssh tunnel requested");

        self.ensure()?;

        let args: CloseSshTunnelArgs = unpack_args!("close_ssh_tunnel", args)?;

        let control_socket_path = control_socket_path!(&args.tunnel_id);

        log::debug!(
            "Closing ssh tunnel \"{}\", socket path: \"{}\"",
            args.tunnel_id,
            control_socket_path.display()
        );

        Self::close_tunnel_command(&control_socket_path).await?;

        Ok(None)
    }

    async fn close_tunnel_command(control_socket_path: &Path) -> Result<()> {
        let result = exec_as_tunnel_user("ssh")
            .stdout(Stdio::piped())
            .args(["-O", "exit"])
            .args(["-S", control_socket_path.to_string_lossy().as_ref()])
            .args(["bastion_host"]) // the destination host name is not used but necessary for the ssh command here
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("close_ssh_tunnel: {e}"))?;

        if !result.status.success() {
            warn!(
                "Unexpected error upon closing tunnel: {}",
                str::from_utf8(&result.stderr).unwrap()
            );
        }

        Ok(())
    }
}

async fn store_ssh_cert(cert_path: &Path, data: &str) -> Result<()> {
    let mut child = exec_as_tunnel_user("tee")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg(&*cert_path.to_string_lossy())
        .spawn()
        .map_err(|_e| anyhow::anyhow!("failed to store ssh certificate"))?;

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(data.as_bytes()).await?;
    drop(stdin); // necessary to close stdin

    if !child.wait().await?.success() {
        error!("failed to store ssh certificate");
        anyhow::bail!("Error on storing ssh certificate");
    }

    Ok(())
}

// RAII handle to the temporary bastion host certificate
struct SshCredentialsGuard {
    key_path: PathBuf,
}

impl SshCredentialsGuard {
    fn new(key_path: &Path) -> Result<SshCredentialsGuard> {
        anyhow::ensure!(key_path.parent().is_some());

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

impl Drop for SshCredentialsGuard {
    fn drop(&mut self) {
        [self.pub_key(), self.priv_key(), self.cert()]
            .iter()
            .for_each(|file| {
                let result = exec_as_tunnel_user("rm").arg(file).spawn();

                if let Err(err) = result {
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
            .unwrap(),
        )
        .set_content_type("application/json")
        .set_content_encoding("utf-8")
        .build()
        .unwrap();

    tx_outgoing_message
        .send(msg)
        .await
        .context("notify_tunnel_termination")
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn validate_correct_uuid() {
        let uuid = "c0c7d30c-511a-4fed-80b2-b4cccc74228e";
        let uuid_message = format!("{{\"tunnel_id\": \"{uuid}\"}}");

        let args = serde_json::from_str::<GetSshPubKeyArgs>(&uuid_message).unwrap();

        assert_eq!(args.tunnel_id, uuid);
    }

    #[test]
    #[should_panic]
    fn validate_incorrect_uuid() {
        let uuid = "!@#$S";
        let uuid_message = format!("{{\"tunnel_id\": \"{uuid}\"}}");

        let _args = serde_json::from_str::<GetSshPubKeyArgs>(&uuid_message).unwrap();
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
}
