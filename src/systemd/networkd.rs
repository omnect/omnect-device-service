use anyhow::{Context, Result, bail, ensure};
use freedesktop_entry_parser::parse_entry;
use regex_lite::RegexBuilder;
use std::{env, time::Duration};

macro_rules! service_file_path {
    () => {
        env::var("WAIT_ONLINE_SERVICE_FILE_PATH")
            .unwrap_or("/lib/systemd/system/systemd-networkd-wait-online.service".to_string())
    };
}

macro_rules! env_file_path {
    () => {
        env::var("ENV_FILE_PATH")
            .unwrap_or("/etc/omnect/systemd-networkd-wait-online.env".to_string())
    };
}

#[cfg(not(feature = "mock"))]
pub async fn networkd_interfaces() -> Result<serde_json::Value> {
    use log::debug;

    let result = zbus::Connection::system()
        .await
        .context("networkd_interfaces: zbus::Connection::system() failed")?
        .call_method(
            Some("org.freedesktop.network1"),
            "/org/freedesktop/network1",
            Some("org.freedesktop.network1.Manager"),
            "Describe",
            &(),
        )
        .await
        .context("networkd_interfaces: call_method() failed")?;

    debug!("networkd_interfaces: succeeded");
    serde_json::from_str(
        result
            .body()
            .deserialize()
            .context("networkd_interfaces: cannot deserialize body")?,
    )
    .context("networkd_interfaces: cannot parse network description")
}

#[cfg(feature = "mock")]
pub async fn networkd_interfaces() -> Result<serde_json::Value> {
    crate::common::from_json_file("testfiles/positive/systemd-networkd-link-description.json")
}

pub fn networkd_wait_online_timeout() -> Result<Option<Duration>> {
    /*
       we expect systemd-networkd-wait-online.service file to be present.
       we expect the following entries:
           - EnvironmentFile=-/etc/omnect/systemd-networkd-wait-online.env
           - ExecStart= ... --timeout=${OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS:-300}
    */
    let service_file_path = &service_file_path!();
    let env_file_path = &env_file_path!();

    let Ok(entry) = parse_entry(service_file_path) else {
        bail!("service file missing: {service_file_path}");
    };

    let Some(exec_start_entry) = entry.section("Service").attr("ExecStart") else {
        bail!("ExecStart entry missing");
    };

    ensure!(
        exec_start_entry.contains("--timeout=${OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS:-300}"),
        "unexpected timeout config in ExecStart: {exec_start_entry}"
    );

    let Some(env_file_entry) = entry.section("Service").attr("EnvironmentFile") else {
        bail!("EnvironmentFile entry missing");
    };

    ensure!(
        env_file_entry.contains(env_file_path),
        "unexpected path in EnvironmentFile: {env_file_entry}"
    );

    let default_result = Ok(Some(Duration::from_secs(300)));

    let Ok(env) = std::fs::read_to_string(env_file_path) else {
        return default_result;
    };

    let re = RegexBuilder::new(r#"^OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS="(.*)""#)
        .multi_line(true)
        .build()
        .context("cannot build regex to get OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS")?;

    let Some(caps) = re.captures(&env) else {
        return default_result;
    };

    let (_, [secs]) = caps.extract();

    let timeout = secs
        .parse::<u64>()
        .context("cannot parse OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS")?;

    if 0 < timeout {
        return Ok(Some(Duration::from_secs(timeout)));
    }

    Ok(None)
}

pub fn set_networkd_wait_online_timeout(timeout: Option<Duration>) -> Result<()> {
    let timeout = match timeout {
        Some(t) if t == Duration::from_secs(0) => None,
        _ => timeout,
    };

    if timeout != networkd_wait_online_timeout()? {
        let env_file_path = &env_file_path!();
        let timeout = timeout.unwrap_or_default().as_secs().to_string();
        let timeout = format!("OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS=\"{timeout}\"");

        let env = if let Ok(env) = std::fs::read_to_string(env_file_path) {
            let re = RegexBuilder::new(r#"^OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS=".*""#)
                .multi_line(true)
                .build()
                .context("cannot build regex to set OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS")?;
            let result = re.replace(env.as_str(), &timeout);

            // append since nothing captured and replaced
            if env.as_str() == result {
                format!("{env}\n{timeout}")
            } else {
                result.into_owned()
            }
        } else {
            // file didn't exist
            timeout
        };

        std::fs::write(env_file_path, env).context("cannot write env file")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn networkd_wait_online_timeout_invalid_input() {
        crate::common::set_env_var("WAIT_ONLINE_SERVICE_FILE_PATH", "");
        assert!(
            networkd_wait_online_timeout()
                .unwrap_err()
                .to_string()
                .starts_with("service file missing")
        );
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/negative/systemd-networkd-wait-online-no-execstart.service",
        );
        assert!(
            networkd_wait_online_timeout()
                .unwrap_err()
                .to_string()
                .starts_with("ExecStart entry missing")
        );
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/negative/systemd-networkd-wait-online-invalid-timeout.service",
        );
        assert!(
            networkd_wait_online_timeout()
                .unwrap_err()
                .to_string()
                .starts_with("unexpected timeout config in ExecStart")
        );
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/negative/systemd-networkd-wait-online-no-envfile.service",
        );
        assert!(
            networkd_wait_online_timeout()
                .unwrap_err()
                .to_string()
                .starts_with("EnvironmentFile entry missing")
        );
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/negative/systemd-networkd-wait-online-invalid-envfile.service",
        );
        assert!(
            networkd_wait_online_timeout()
                .unwrap_err()
                .to_string()
                .starts_with("unexpected path in EnvironmentFile")
        );

        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/negative/systemd-networkd-wait-online-invalid-default-timeout.service",
        );
        crate::common::set_env_var(
            "ENV_FILE_PATH",
            "testfiles/negative/systemd-networkd-wait-online-invalid-timeout.env",
        );
        assert!(
            networkd_wait_online_timeout()
                .unwrap_err()
                .to_string()
                .starts_with("cannot parse OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS")
        );

        crate::common::remove_env_var("WAIT_ONLINE_SERVICE_FILE_PATH");
        crate::common::remove_env_var("ENV_FILE_PATH");
    }

    #[test]
    fn networkd_wait_online_timeout_ok() {
        let _ = fs::remove_file("/tmp/systemd-networkd-wait-online1.env");
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/positive/systemd-networkd-wait-online-no-envfile.service",
        );
        crate::common::set_env_var("ENV_FILE_PATH", "/tmp/systemd-networkd-wait-online1.env");
        assert_eq!(
            networkd_wait_online_timeout().unwrap(),
            Some(Duration::from_secs(300))
        );
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/positive/systemd-networkd-wait-online-env1.service",
        );
        crate::common::set_env_var(
            "ENV_FILE_PATH",
            "testfiles/positive/systemd-networkd-wait-online1.env",
        );
        assert_eq!(
            networkd_wait_online_timeout().unwrap(),
            Some(Duration::from_secs(1))
        );
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/positive/systemd-networkd-wait-online-env2.service",
        );
        crate::common::set_env_var(
            "ENV_FILE_PATH",
            "testfiles/positive/systemd-networkd-wait-online2.env",
        );
        assert_eq!(networkd_wait_online_timeout().unwrap(), None);
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/positive/systemd-networkd-wait-online-env3.service",
        );
        crate::common::set_env_var(
            "ENV_FILE_PATH",
            "testfiles/positive/systemd-networkd-wait-online3.env",
        );
        assert_eq!(
            networkd_wait_online_timeout().unwrap(),
            Some(Duration::from_secs(300))
        );

        crate::common::remove_env_var("WAIT_ONLINE_SERVICE_FILE_PATH");
        crate::common::remove_env_var("ENV_FILE_PATH");
    }

    #[test]
    fn set_networkd_wait_online_timeout_ok() {
        let _ = fs::remove_file("/tmp/systemd-networkd-wait-online1.env");
        crate::common::set_env_var(
            "WAIT_ONLINE_SERVICE_FILE_PATH",
            "testfiles/positive/systemd-networkd-wait-online-no-envfile.service",
        );
        crate::common::set_env_var("ENV_FILE_PATH", "/tmp/systemd-networkd-wait-online1.env");
        assert!(set_networkd_wait_online_timeout(Some(Duration::from_secs(100))).is_ok());
        assert_eq!(
            fs::read_to_string("/tmp/systemd-networkd-wait-online1.env").unwrap(),
            "OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS=\"100\""
        );
        assert!(set_networkd_wait_online_timeout(None).is_ok());
        assert_eq!(
            fs::read_to_string("/tmp/systemd-networkd-wait-online1.env").unwrap(),
            "OMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS=\"0\""
        );

        std::fs::write("/tmp/systemd-networkd-wait-online1.env", "env=1").unwrap();
        assert!(set_networkd_wait_online_timeout(None).is_ok());
        assert_eq!(
            fs::read_to_string("/tmp/systemd-networkd-wait-online1.env").unwrap(),
            "env=1\nOMNECT_WAIT_ONLINE_TIMEOUT_IN_SECS=\"0\""
        );

        crate::common::remove_env_var("WAIT_ONLINE_SERVICE_FILE_PATH");
        crate::common::remove_env_var("ENV_FILE_PATH");
    }
}
