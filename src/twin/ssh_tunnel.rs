use super::Feature;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::executor;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::any::Any;
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use std::ops::Drop;

static SSH_KEY_TYPE: &str = "ed25519";
static SSH_PORT: u16 = 22;

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

fn create_key_pair(priv_key_path: &Path) -> Result<()> {
    // check if there is already a key pair, if not, create it first
    fs::create_dir_all(
        priv_key_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid key path: {}", priv_key_path.display()))?,
    )?;

    let result = std::process::Command::new("ssh-keygen")
        .stdout(Stdio::piped())
        .args(["-q"])
        .args(["-f", priv_key_path.to_string_lossy().as_ref()])
        .args(["-t", SSH_KEY_TYPE])
        .args(["-N", ""])
        .output();

    let output = result.map_err(|err| {
        error!("Failed to create ssh key pair: {}", err);
        anyhow::anyhow!("Error on ssh key creation.")
    })?;

    if !output.status.success() {
        error!(
            "Failed to create ssh key pair: {}",
            str::from_utf8(&output.stderr)?
        );
        anyhow::bail!("Error on ssh key creation.");
    }

    Ok(())
}

fn store_ssh_cert(cert_path: &Path, data: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .open(cert_path)?;
    f.write_all(data.as_bytes())?;

    Ok(())
}

pub struct SshTunnel {
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
}

impl SshTunnel {
    const SSH_TUNNEL_VERSION: u8 = 1;
    const ID: &'static str = "ssh_tunnel";

    pub fn new() -> Self {
        SshTunnel {
        }
    }

    pub async fn get_ssh_pub_key(
        &self,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("ssh pub key requested");

        self.ensure()?;

        #[derive(Deserialize)]
        struct GetSshPubKeyArgs {
            tunnel_id: String,
        }

        let args: GetSshPubKeyArgs =
            serde_json::from_value(args).map_err(|e| anyhow::anyhow!("get_ssh_pub_key: {e}"))?;

        let tunnel_id = Uuid::parse_str(&args.tunnel_id)?
            .as_hyphenated()
            .to_string();

        let (priv_key_path, pub_key_path) = (priv_key_path!(tunnel_id), pub_key_path!(tunnel_id));

        create_key_pair(&priv_key_path).map_err(|e| anyhow::anyhow!("get_ssh_pub_key: {e}"))?;

        let pub_key = fs::read_to_string(pub_key_path.to_string_lossy().as_ref())
            .map_err(|err| anyhow::anyhow!("get_ssh_pub_key: {err}"))?;

        Ok(Some(json!({ "key": pub_key })))
    }

    pub async fn open_ssh_tunnel(
        &self,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        info!("open ssh tunnel requested");

        self.ensure()?;

        #[derive(Deserialize)]
        struct OpenSshTunnelArgs {
            tunnel_id: String,
            certificate: String,
            #[serde(rename = "host")]
            bastion_host: String,
            #[serde(rename = "port")]
            bastion_port: u16,
            #[serde(rename = "user")]
            bastion_user: String,
            #[serde(rename = "socket_path")]
            bastion_socket_path: String,
        }

        let args: OpenSshTunnelArgs =
            serde_json::from_value(args).map_err(|e| anyhow::anyhow!("open_ssh_tunnel: {e}"))?;

        let tunnel_id = Uuid::parse_str(&args.tunnel_id)
            .map_err(|e| anyhow::anyhow!("open_ssh_tunnel: {e}"))?
            .as_hyphenated()
            .to_string();

        // ensure our ssh keys and certificate are cleaned up properly when
        // leaving this function
        let ssh_creds = SshCredentialsGuard::new(&priv_key_path!(&tunnel_id))
            .map_err(|e| anyhow::anyhow!("open_ssh_tunnel: {e}"))?;

        // store the certificate so that ssh can use it for login on the bastion host
        store_ssh_cert(&ssh_creds.cert(), &args.certificate)?;

        log::debug!(
            "Starting ssh tunnel \"{}\" bastion host: \"{}:{}\", bastion user: \"{}\"",
            tunnel_id,
            args.bastion_host,
            args.bastion_port,
            args.bastion_user
        );

        // now spawn the ssh tunnel: we tell ssh to allocate a reverse tunnel on the
        // bastion host bound to a socket file there. Furthermore, we tell ssh to 1)
        // echo once the connection was successful, so that we can check from here
        // when we can signal a successful connection, 2) we tell it to sleep. The
        // latter is so that the connection is closed after it remains unused for
        // some time.
        let mut ssh_command = Command::new("ssh");
        ssh_command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(["-M"])
            .args([
                "-S",
                control_socket_path!(&tunnel_id).to_string_lossy().as_ref(),
            ])
            .args(["-i", ssh_creds.priv_key().to_string_lossy().as_ref()])
            .args([
                "-o",
                &format!("CertificateFile={}", ssh_creds.cert().to_string_lossy()),
            ])
            .args([
                "-R",
                &format!(
                    "{}/{}:localhost:{}",
                    args.bastion_socket_path, tunnel_id, SSH_PORT
                ),
            ])
            .args([&format!("{}@{}", args.bastion_user, args.bastion_host)])
            .args(["-p", &format!("{}", args.bastion_port)])
            .args(["-o", "ExitOnForwardFailure=yes"]) // ensure ssh terminates if anything goes south
            .args(["-o", "StrictHostKeyChecking=no"]) // allow bastion host to be redeployed
            .args(["-o", "UserKnownHostsFile=/dev/null"]);

        let mut ssh_process = ssh_command
            .spawn()
            .map_err(|e| anyhow::anyhow!("open_ssh_tunnel: {e}"))?;

        let stdout = ssh_process.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout).lines();

        let spawn_id = tunnel_id.clone();
        tokio::spawn(async move {
            info!("Waiting for connection to close: {}", spawn_id);
            let output = match ssh_process.wait_with_output().await {
                Ok(output) => output,
                Err(err) => {
                    error!("Could not retrieve output from ssh process: {}", err);
                    return;
                }
            };

            if !output.status.success() {
                error!(
                    "Failed to establish ssh tunnel: {}",
                    str::from_utf8(&output.stderr).unwrap()
                );
                return;
            }

            info!("Closed ssh tunnel: {}", spawn_id);
        });

        let response = executor::block_on(reader.next_line());
        match response {
            Ok(Some(msg)) => {
                if msg == "established" {
                    // Tunnel is ready to be used, return
                    log::debug!(
                        "Successfully established connection \"{}\" to \"{}:{}\"",
                        tunnel_id,
                        args.bastion_host,
                        args.bastion_port
                    );

                    Ok(None)
                } else {
                    warn!("Got unexpected response from ssh server: {}", msg);
                    anyhow::bail!("open_ssh_tunnel: Failed to establish ssh tunnel");
                }
            }
            Ok(None) => {
                // async task takes care of handling stderr
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

        #[derive(Deserialize)]
        struct CloseSshTunnelArgs {
            tunnel_id: String,
        }

        let args: CloseSshTunnelArgs =
            serde_json::from_value(args).map_err(|e| anyhow::anyhow!("close_ssh_tunnel: {e}"))?;

        let tunnel_id = Uuid::parse_str(&args.tunnel_id)?
            .as_hyphenated()
            .to_string();

        let control_socket_path = control_socket_path!(&tunnel_id);

        log::debug!(
            "Closing ssh tunnel \"{}\", socket path: \"{}\"",
            tunnel_id,
            control_socket_path.display()
        );

        let result = std::process::Command::new("ssh")
            .stdout(Stdio::piped())
            .args(["-O", "exit"])
            .args(["-S", control_socket_path.to_string_lossy().as_ref()])
            .args(["bastion_host"]) // the destination host name is not used but necessary for the ssh command here
            .output()
            .map_err(|e| anyhow::anyhow!("close_ssh_tunnel: {e}"))?;

        if !result.status.success() {
            warn!(
                "Unexpected error upon closing tunnel \"{}\": {}",
                tunnel_id,
                str::from_utf8(&result.stderr).unwrap()
            );
        }

        Ok(None)
    }
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
                if let Err(err) = fs::remove_file(file) {
                    warn!(
                        "Failed to delete certificate \"{}\": {}",
                        file.to_string_lossy(),
                        err
                    );
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

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
