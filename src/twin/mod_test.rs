#[cfg(test)]
#[allow(clippy::module_inception)]
pub mod mod_test {
    use super::super::*;
    use crate::{consent_path, history_consent_path};
    use azure_iot_sdk::client::{AuthenticationObserver, DirectMethodObserver, TwinObserver};
    use cp_r::CopyOptions;
    use env_logger::{Builder, Env};
    use futures_executor::block_on;
    use lazy_static::lazy_static;
    use mockall::{automock, predicate::*};
    use rand::{
        distributions::Alphanumeric,
        {thread_rng, Rng},
    };
    use regex::Regex;
    use serde_json::json;
    use std::fs::{copy, create_dir_all, remove_dir_all};
    use std::{env, fs::OpenOptions, path::PathBuf, process::Command, time::Duration};
    use strum::EnumCount;

    lazy_static! {
        static ref LOG: () = if cfg!(debug_assertions) {
            Builder::from_env(Env::default().default_filter_or("debug")).init()
        } else {
            Builder::from_env(Env::default().default_filter_or("info")).init()
        };
    }

    const TMPDIR_FORMAT_STR: &str = "/tmp/omnect-device-service-tests/";
    const UTC_REGEX: &str = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[\+-]\d{2}:\d{2})";

    pub struct MyIotHubBuilder;

    impl MyIotHubBuilder {
        pub fn build_module_client(&self, _connection_string: &str) -> Result<MockMyIotHub> {
            Ok(MockMyIotHub::default())
        }

        pub fn observe_connection_state(
            self,
            _tx_connection_status: AuthenticationObserver,
        ) -> Self {
            self
        }

        pub fn observe_desired_properties(self, _tx_twin_desired: TwinObserver) -> Self {
            self
        }

        pub fn observe_direct_methods(self, _tx_direct_method: DirectMethodObserver) -> Self {
            self
        }
    }

    #[allow(dead_code)]
    struct MyIotHub {}

    #[automock]
    impl MyIotHub {
        #[allow(dead_code)]
        pub fn builder() -> MyIotHubBuilder {
            MyIotHubBuilder {}
        }

        #[allow(dead_code)]
        pub fn sdk_version_string() -> String {
            "".to_string()
        }

        #[allow(dead_code)]
        pub fn twin_report(&self, _reported: serde_json::Value) -> Result<()> {
            Ok(())
        }

        #[allow(dead_code)]
        pub fn send_d2c_message(&self, mut _message: IotMessage) -> Result<()> {
            Ok(())
        }

        #[allow(dead_code)]
        pub async fn shutdown(&mut self) {}
    }

    pub struct TestEnvironment {
        dirpath: std::string::String,
    }

    impl TestEnvironment {
        pub fn new(name: &str) -> TestEnvironment {
            lazy_static::initialize(&LOG);
            let dirpath = format!("{}{}", TMPDIR_FORMAT_STR, name);
            create_dir_all(&dirpath).unwrap();
            TestEnvironment { dirpath }
        }

        pub fn copy_directory(&self, dir: &str) -> PathBuf {
            let destdir = String::from(dir);
            let destdir = destdir.split('/').last().unwrap();
            let path = PathBuf::from(format!("{}/{}", self.dirpath, destdir));
            CopyOptions::new().copy_tree(dir, &path).unwrap();
            path
        }

        pub fn mkdir(&self, dir: &str) -> PathBuf {
            let destdir = String::from(dir);
            let destdir = destdir.split('/').last().unwrap();
            let path = PathBuf::from(format!("{}/{}", self.dirpath, destdir));
            std::fs::create_dir_all(&path).unwrap();
            path
        }

        pub fn copy_file(&self, file: &str) -> PathBuf {
            let destfile = String::from(file);
            let destfile = destfile.split('/').last().unwrap();
            let path = PathBuf::from(format!("{}/{}", self.dirpath, destfile));
            copy(file, &path).unwrap();
            path
        }

        pub fn dirpath(&self) -> String {
            self.dirpath.clone()
        }
    }

    impl Drop for TestEnvironment {
        fn drop(&mut self) {
            // place your cleanup code here
            remove_dir_all(&self.dirpath).unwrap_or_else(|e| {
                // ignore all errors if dir cannot be deleted
                error!("cannot remove_dir_all: {e:#}");
            });
        }
    }

    struct TestCase;

    struct TestConfig {
        twin: Twin,
        dir: PathBuf,
    }

    impl TestCase {
        fn run(
            test_files: Vec<&str>,
            test_dirs: Vec<&str>,
            env_vars: Vec<(&str, &str)>,
            set_mock_expectations: impl Fn(&mut MockMyIotHub),
            run_test: impl Fn(&mut TestConfig),
        ) {
            // unique testcase name
            let testcase_name: String = thread_rng()
                .sample_iter(&Alphanumeric)
                .take(10)
                .map(char::from)
                .collect();

            // create unique folder under /tmp/
            let test_env = TestEnvironment::new(testcase_name.as_str());

            // copy test files and dirs
            test_env.copy_file("testfiles/positive/systemd-networkd-wait-online.service");
            test_env.copy_file("testfiles/positive/factory-reset.json");
            test_env.copy_file("testfiles/positive/factory-reset-status_succeeded");
            test_env.copy_file("testfiles/positive/config.toml.est");
            test_env.copy_file("testfiles/positive/deviceid1-bd732105ef89cf8edd2606a5309c8a26b7b5599a4e124a0fe6199b6b2f60e655.cer");
            test_files.iter().for_each(|file| {
                test_env.copy_file(file);
            });

            test_dirs.iter().for_each(|dir| {
                test_env.copy_directory(dir);
            });

            test_env.mkdir("empty-dir");

            // set env vars
            env::set_var("SSH_TUNNEL_DIR_PATH", test_env.dirpath().as_str());
            env::set_var("OS_RELEASE_DIR_PATH", test_env.dirpath().as_str());
            env::set_var("CONSENT_DIR_PATH", test_env.dirpath().as_str());
            env::set_var("WPA_SUPPLICANT_DIR_PATH", test_env.dirpath().as_str());
            env::set_var(
                "WAIT_ONLINE_SERVICE_FILE_PATH",
                format!(
                    "{}/systemd-networkd-wait-online.service",
                    test_env.dirpath()
                ),
            );
            env::set_var("FACTORY_RESET_CONFIG_FILE_PATH", format!("{}/factory-reset.json", test_env.dirpath()));
            env::set_var(
                "FACTORY_RESET_STATUS_FILE_PATH",
                format!("{}/factory-reset-status_succeeded", test_env.dirpath()),
            );
            env::set_var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH", format!("{}/empty-dir", test_env.dirpath()));
            env::set_var("CONNECTION_STRING", "my-constr");
            env::set_var(
                "IDENTITY_CONFIG_FILE_PATH",
                format!("{}/config.toml.est", test_env.dirpath()),
            );
            env::set_var(
                "EST_CERT_FILE_PATH",
                format!("{}/deviceid1-bd732105ef89cf8edd2606a5309c8a26b7b5599a4e124a0fe6199b6b2f60e655.cer", test_env.dirpath()),
            );

            env_vars.iter().for_each(|env| env::set_var(env.0, env.1));

            // create iothub client mock
            let ctx = MockMyIotHub::builder_context();
            ctx.expect().times(1).returning(|| MyIotHubBuilder {});
            let ctx = MockMyIotHub::sdk_version_string_context();
            ctx.expect().returning(|| {
                use azure_iot_sdk::client::IotHubClient as Client;
                Client::sdk_version_string()
            });

            let (tx_connection_status, _rx_connection_status) = mpsc::channel(100);
            let (tx_twin_desired, _rx_twin_desired) = mpsc::channel(100);
            let (tx_direct_method, _rx_direct_method) = mpsc::channel(100);
            let (tx_reported_properties, mut rx_reported_properties) = mpsc::channel(100);
            let (tx_outgoing_message, _rx_outgoing_message) = mpsc::channel(100);
            let (tx_web_service, _rx_web_service) = mpsc::channel(100);

            let mut twin = block_on(Twin::new(
                tx_web_service,
                tx_reported_properties,
                tx_outgoing_message,
            ))
            .unwrap();

            let client_builder = IotHubClient::builder()
                .observe_connection_state(tx_connection_status)
                .observe_desired_properties(tx_twin_desired)
                .observe_direct_methods(tx_direct_method);
            twin.client = Some(
                client_builder
                    .build_module_client("connection_string")
                    .unwrap(),
            );

            // create test config
            let mut config = TestConfig {
                twin,
                dir: PathBuf::from(test_env.dirpath()),
            };

            // set testcase specific mock expectaions
            set_mock_expectations(&mut config.twin.client.as_mut().unwrap());

            // run test
            run_test(&mut config);

            // compute reported properties
            while let Ok(val) = rx_reported_properties.try_recv() {
                info!("{val:?}");
                config
                    .twin
                    .client
                    .as_ref()
                    .unwrap()
                    .twin_report(val)
                    .unwrap()
            }

            // cleanup env vars
            env::remove_var("SSH_TUNNEL_DIR_PATH");
            env::remove_var("OS_RELEASE_DIR_PATH");
            env::remove_var("CONSENT_DIR_PATH");
            env::remove_var("WPA_SUPPLICANT_DIR_PATH");
            env::remove_var("WAIT_ONLINE_SERVICE_FILE_PATH");
            env::remove_var("FACTORY_RESET_CONFIG_FILE_PATH");
            env::remove_var("FACTORY_RESET_STATUS_FILE_PATH");
            env::remove_var("FACTORY_RESET_CUSTOM_CONFIG_DIR_PATH");
            env::remove_var("IDENTITY_CONFIG_FILE_PATH");
            env::remove_var("EST_CERT_FILE_PATH");
            env_vars.iter().for_each(|e| env::remove_var(e.0));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn init_features_ok_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .with(eq(json!({"system_info":{"version":1,}})))
                .times(2)
                .returning(|_| Ok(()));
            let s = IotHubClient::sdk_version_string();
            mock.expect_twin_report()
                .with(eq(json!({"system_info":{
                    "omnect_device_service_version": env!("CARGO_PKG_VERSION"),
                    "azure_sdk_version": s,
                    "boot_time": null,
                    "os": {
                        "name": "OMNECT-gateway-devel",
                        "version": "4.0.17.123456"
                    }
                }})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"factory_reset":{"version":2}})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"ssh_tunnel":{"version":1}})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"device_update_consent":{"version":1}})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"modem_info":null})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"network_status":{"version":3}})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({
                    "network_status":{
                        "interfaces": [{
                            "ipv4": {
                                "addrs": [{
                                    "addr": "192.168.0.74",
                                    "dhcp": true,
                                    "prefix_len": 24
                                }],
                                "dns": ["192.168.0.1"],
                                "gateways": ["192.168.0.1"]
                            },
                            "mac": "80:45:244:53:22:37",
                            "name": "eth0",
                            "online": true
                        },
                        {
                            "ipv4": {
                                "addrs": [],
                                "dns": [],
                                "gateways": []
                            },
                            "mac": "80:45:244:44:14:81",
                            "name": "eth1",
                            "online": false
                        }]
                    }
                })))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"reboot":{"version":2}})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({ "wifi_commissioning": null })))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(
                    json!({"wait_online_timeout_secs":{"nanos":0, "secs": 300}}),
                ))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(
                    json!({"device_update_consent":{"general_consent":["swupdate"], "reset_consent_on_fail": false}}),
                ))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"device_update_consent":{"user_consent_request":[{"swupdate":"<version>"}]}})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"device_update_consent":{"user_consent_history":{"swupdate":["<version>"]}}})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .withf(|val| {
                    let reported = format!("{:?}", val);
                    let reported = reported.as_str();

                    let re = format!(
                        "{}{}{}",
                        regex::escape(
                            r#"Object {"factory_reset": Object {"status": Object {"date": String(""#,
                        ),
                        UTC_REGEX,
                        regex::escape(r#""), "status": String("succeeded")}}}"#,),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(2)
                .returning(|_| Ok(()));

                mock.expect_twin_report()
                .with(eq(
                    json!({"factory_reset":{"keys":["certificates", "firewall", "network"]}}),
                ))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"provisioning_config":{"version": 1}})))
                .times(2)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({
                    "provisioning_config":{
                        "source": "dps",
                        "method": {
                            "x509": {
                                "expires": "2024-10-10T11:27:38Z",
                                "est": true,
                            }
                        }
                    }
                })))
                .times(2)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &'_ mut TestConfig| {
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());
        };

        TestCase::run(test_files, vec![], vec![], expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn init_features_no_factory_reset_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];
        let env_vars = vec![("SUPPRESS_FACTORY_RESET", "true")];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
            .with(eq(json!({"system_info":{"version":1,}})))
            .times(1)
            .returning(|_| Ok(()));
        let s = IotHubClient::sdk_version_string();
        mock.expect_twin_report()
            .with(eq(json!({"system_info":{
                "omnect_device_service_version": env!("CARGO_PKG_VERSION"),
                "azure_sdk_version": s,
                "boot_time": null,
                "os": {
                    "name": "OMNECT-gateway-devel",
                    "version": "4.0.17.123456"
                }
            }})))
            .times(1)
            .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({ "factory_reset": null })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"ssh_tunnel":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"device_update_consent":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"modem_info":null})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"network_status":{"version": 3}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({
                    "network_status":{
                        "interfaces": [{
                            "ipv4": {
                                "addrs": [{
                                    "addr": "192.168.0.74",
                                    "dhcp": true,
                                    "prefix_len": 24
                                }],
                                "dns": ["192.168.0.1"],
                                "gateways": ["192.168.0.1"]
                            },
                            "mac": "80:45:244:53:22:37",
                            "name": "eth0",
                            "online": true
                        },
                        {
                            "ipv4": {
                                "addrs": [],
                                "dns": [],
                                "gateways": []
                            },
                            "mac": "80:45:244:44:14:81",
                            "name": "eth1",
                            "online": false
                        }]
                    }
                })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"reboot":{"version":2}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({ "wifi_commissioning": null })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(
                    json!({"wait_online_timeout_secs":{"nanos":0, "secs": 300}}),
                ))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(
                    json!({"device_update_consent":{"general_consent":["swupdate"], "reset_consent_on_fail": false}}),
                ))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"device_update_consent":{"user_consent_request":[{"swupdate":"<version>"}]}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"device_update_consent":{"user_consent_history":{"swupdate":["<version>"]}}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"provisioning_config":{"version": 1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({
                    "provisioning_config":{
                        "source": "dps",
                        "method": {
                            "x509": {
                                "expires": "2024-10-10T11:27:38Z",
                                "est": true,
                            }
                        }
                    }
                })))
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &'_ mut TestConfig| {
            assert!(test_attr.twin.feature::<FactoryReset>().is_err());
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wifi_commissioning_feature_test() {
        let test_files = vec!["testfiles/wifi_commissioning/os-release"];
        let env_vars = vec![
            ("SUPPRESS_SYSTEM_INFO", "true"),
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            ("SUPPRESS_SSH_TUNNEL", "true"),
            ("SUPPRESS_PROVISIONING_CONFIG", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 3)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &'_ mut TestConfig| {
            assert!(test_attr.twin.feature::<FactoryReset>().is_err());
            assert!(test_attr.twin.feature::<DeviceUpdateConsent>().is_err());
            assert!(test_attr.twin.feature::<NetworkStatus>().is_err());
            assert!(test_attr.twin.feature::<Reboot>().is_err());
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_twin_test() {
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
        ];

        let expect = |_mock: &mut MockMyIotHub| {};

        let test = |test_attr: &'_ mut TestConfig| {
            assert!(test_attr
                .twin
                .handle_desired(TwinUpdateState::Partial, json!(""))
                .is_ok());

            assert_eq!(
                test_attr
                    .twin
                    .handle_desired(TwinUpdateState::Partial, json!({"general_consent": {}}))
                    .unwrap_err()
                    .to_string(),
                "feature disabled: device_update_consent"
            );

            assert_eq!(
                test_attr
                    .twin
                    .handle_desired(TwinUpdateState::Complete, json!(""))
                    .unwrap_err()
                    .to_string(),
                "handle_desired: 'desired' missing while TwinUpdateState::Complete"
            );
            assert_eq!(
                test_attr
                    .twin
                    .handle_desired(
                        TwinUpdateState::Complete,
                        json!({"desired": {"general_consent": {}}}),
                    )
                    .unwrap_err()
                    .to_string(),
                "feature disabled: device_update_consent"
            );
        };

        TestCase::run(vec![], vec![], env_vars, expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_and_report_general_consent_failed_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/negative/history_consent.json",
        ];
        let env_vars = vec![
            ("SUPPRESS_SYSTEM_INFO", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_SSH_TUNNEL", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            ("SUPPRESS_PROVISIONING_CONFIG", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report().times(11).returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            let err = block_on(async { test_attr.twin.connect_twin().await }).unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("report_user_consent: serde_json::from_reader")));
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_and_report_general_consent_passed_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 12)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({
                    "device_update_consent": {
                        "general_consent": ["swupdate1", "swupdate2"],
                        "reset_consent_on_fail": false
                    }
                })))
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            let file_content = || -> serde_json::Value {
                serde_json::from_reader(
                    OpenOptions::new()
                        .read(true)
                        .create(false)
                        .open(test_attr.dir.join("consent_conf.json"))
                        .unwrap(),
                )
                .unwrap()
            };

            assert_json_diff::assert_json_eq!(
                file_content(),
                json!({
                    "general_consent": ["swupdate"],
                    "reset_consent_on_fail": false
                })
            );

            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());

            assert_json_diff::assert_json_eq!(
                file_content(),
                json!({
                    "general_consent": ["swupdate"],
                    "reset_consent_on_fail": false
                })
            );

            assert!(test_attr
                .twin
                .handle_desired(TwinUpdateState::Complete, json!({"desired": {}}))
                .is_ok());

            assert_json_diff::assert_json_eq!(
                file_content(),
                json!({
                    "general_consent": ["swupdate"],
                    "reset_consent_on_fail": false
                })
            );

            assert!(test_attr
                .twin
                .handle_desired(
                    TwinUpdateState::Partial,
                    json!({"general_consent": ["SWUPDATE2", "SWUPDATE1"]}),
                )
                .is_ok());

            assert_json_diff::assert_json_eq!(
                file_content(),
                json!({
                    "general_consent": ["swupdate1", "swupdate2"],
                    "reset_consent_on_fail": false
                })
            );

            assert!(test_attr
                .twin
                .handle_desired(TwinUpdateState::Complete, json!({"desired": {}}))
                .is_ok());
        };

        TestCase::run(test_files, vec![], vec![], expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn consent_observe_history_file_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 12)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({
                    "device_update_consent": {
                        "user_consent_history": {
                            "swupdate": [
                                "1.2.3"
                            ]
                        }
                    }
                })))
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());

            serde_json::to_writer_pretty(
                OpenOptions::new()
                    .write(true)
                    .create(false)
                    .truncate(true)
                    .open(history_consent_path!())
                    .unwrap(),
                &json!({
                    "user_consent_history": {
                        "swupdate": [
                            "1.2.3"
                        ]
                    }
                }),
            )
            .unwrap();

            std::thread::sleep(Duration::from_secs(2));
        };

        TestCase::run(test_files, vec![], vec![], expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn consent_direct_method_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let test_dirs = vec!["testfiles/positive/test_component"];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 12)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());

            assert_eq!(
                test_attr
                    .twin
                    .feature::<DeviceUpdateConsent>()
                    .unwrap()
                    .user_consent(json!({
                      "test_component": "1.0.0"
                    }))
                    .unwrap(),
                None
            );
        };

        TestCase::run(test_files, test_dirs, vec![], expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn consent_invalid_component_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let test_dirs = vec!["testfiles/positive/test_component"];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 12)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());

            assert_eq!(
                test_attr
                    .twin
                    .feature::<DeviceUpdateConsent>()
                    .unwrap()
                    .user_consent(json!({
                      "test/_component": "1.0.0"
                    }))
                    .unwrap_err()
                    .to_string(),
                "user_consent: invalid component name: test/_component"
            );

            assert_eq!(
                test_attr
                    .twin
                    .feature::<DeviceUpdateConsent>()
                    .unwrap()
                    .user_consent(json!({
                      "test_component": 1
                    }))
                    .unwrap_err()
                    .to_string(),
                "user_consent: unexpected parameter format"
            );

            assert_eq!(
                test_attr
                    .twin
                    .feature::<DeviceUpdateConsent>()
                    .unwrap()
                    .user_consent(json!({
                      "test_component1": "1.0.0",
                      "test_component2": "1.0.0"
                    }))
                    .unwrap_err()
                    .to_string(),
                "user_consent: unexpected parameter format"
            );

            env::set_var("CONSENT_DIR_PATH", "/../my_path");

            assert_eq!(
                test_attr
                    .twin
                    .feature::<DeviceUpdateConsent>()
                    .unwrap()
                    .user_consent(json!({
                      "test_component": "1.0.0"
                    }))
                    .unwrap_err()
                    .to_string(),
                "user_consent: invalid path /../my_path/test_component/user_consent.json"
            );

            let mut test_dir = test_attr.dir.clone();
            test_dir.push("test_component");
            test_dir.push("..");

            env::set_var("CONSENT_DIR_PATH", &test_dir);

            test_dir.push("test_component");
            test_dir.push("user_consent.json");

            assert_eq!(
                test_attr
                    .twin
                    .feature::<DeviceUpdateConsent>()
                    .unwrap()
                    .user_consent(json!({
                      "test_component": "1.0.0"
                    }))
                    .unwrap_err()
                    .to_string(),
                format!(
                    "user_consent: non-absolute path {}",
                    test_dir.to_str().unwrap()
                )
            );

            let mut test_dir = test_attr.dir.clone();
            test_dir.push("test_component");
            test_dir.push(".");

            env::set_var("CONSENT_DIR_PATH", &test_dir);

            test_dir.push("test_component");
            test_dir.push("user_consent.json");

            assert_eq!(
                test_attr
                    .twin
                    .feature::<DeviceUpdateConsent>()
                    .unwrap()
                    .user_consent(json!({
                      "test_component": "1.0.0"
                    }))
                    .unwrap_err()
                    .to_string(),
                format!("user_consent: invalid path {}", test_dir.to_str().unwrap())
            );
        };

        TestCase::run(test_files, test_dirs, vec![], expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reset_to_factory_settings_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
            "testfiles/positive/wpa_supplicant.conf",
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 12)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .withf(|val| {
                    let reported = format!("{:?}", val);
                    let reported = reported.as_str();

                    let re = format!(
                        "{}{}{}",
                        regex::escape(
                            r#"Object {"factory_reset": Object {"status": Object {"date": String(""#,
                        ),
                        UTC_REGEX,
                        regex::escape(r#""), "status": String("in_progress")}}}"#,),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());

            let factory_reset = test_attr.twin.feature::<FactoryReset>().unwrap();

            assert_eq!(
                block_on(async {
                    factory_reset
                        .reset_to_factory_settings(json!({
                            "preserve": ["applications"]
                        }))
                        .await
                })
                .unwrap_err()
                .to_string(),
                "reset mode missing or not supported"
            );

            assert_eq!(
                block_on(async {
                    factory_reset
                        .reset_to_factory_settings(json!({
                            "mode": 1,
                            "preserve": ["unknown"]
                        }))
                        .await
                })
                .unwrap_err()
                .to_string(),
                "unknown preserve topic received: unknown"
            );

            assert!(block_on(async {
                factory_reset
                    .reset_to_factory_settings(json!({
                        "mode": 1,
                        "preserve": ["network", "firewall", "certificates"]
                    }))
                    .await
            })
            .is_ok());
        };

        TestCase::run(test_files, vec![], vec![], expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn factory_reset_direct_method_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/wpa_supplicant.conf",
        ];
        let env_vars = vec![
            ("SUPPRESS_SYSTEM_INFO", "true"),
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_SSH_TUNNEL", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            ("SUPPRESS_PROVISIONING_CONFIG", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 4)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .withf(|val| {
                    let reported = format!("{:?}", val);
                    let reported = reported.as_str();

                    let re = format!(
                        "{}{}{}",
                        regex::escape(
                            r#"Object {"factory_reset": Object {"status": Object {"date": String(""#,
                        ),
                        UTC_REGEX,
                        regex::escape(r#""), "status": String("succeeded")}}}"#,),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .withf(|val| {
                    let reported = format!("{:?}", val);
                    let reported = reported.as_str();

                    let re = format!(
                        "{}{}{}",
                        regex::escape(
                            r#"Object {"factory_reset": Object {"status": Object {"date": String(""#,
                        ),
                        UTC_REGEX,
                        regex::escape(r#""), "status": String("in_progress")}}}"#,),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());

            let factory_reset = test_attr.twin.feature::<FactoryReset>().unwrap();

            assert_eq!(
                block_on(async {
                    factory_reset
                        .reset_to_factory_settings(json!({
                            "mode": 1,
                            "preserve": ["network"]
                        }))
                        .await
                })
                .unwrap(),
                None
            );
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_ssh_pub_key_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/b7afb216-5f7a-4755-a300-9374f8a0e9ff",
        ];
        let env_vars = vec![
            ("SUPPRESS_SYSTEM_INFO", "true"),
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            ("SUPPRESS_PROVISIONING_CONFIG", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .with(eq(json!({"ssh_tunnel":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report().times(8).returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());

            // test empty tunnel id
            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<SshTunnel>()
                    .unwrap()
                    .get_ssh_pub_key(json!({ "tunnel_id": "" }))
                    .await
            })
            .is_err());

            // test non-uuid tunnel id
            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<SshTunnel>()
                    .unwrap()
                    .get_ssh_pub_key(
                        json!({ "tunnel_id": "So Long, and Thanks for All the Fish 🐬" }),
                    )
                    .await
            })
            .is_err());

            // test creation of pub key
            let tunnel_id: &str = "b054a76d-520c-40a9-b401-0f6bfb7cee9b";
            let pub_key_regex = Regex::new(
                r#"^\{"key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5[0-9A-Za-z+/]+[=]{0,3}(\s.*)\\n"\}?$"#,
            )
            .unwrap();
            let response = block_on(async {
                test_attr
                    .twin
                    .feature::<SshTunnel>()
                    .unwrap()
                    .get_ssh_pub_key(json!({ "tunnel_id": tunnel_id }))
                    .await
                    .unwrap()
                    .unwrap()
                    .to_string()
            });
            assert!(pub_key_regex.is_match(&response));

            // test for correct handling of existing private key file
            let tunnel_id: &str = "b7afb216-5f7a-4755-a300-9374f8a0e9ff";
            let response = block_on(async {
                test_attr
                    .twin
                    .feature::<SshTunnel>()
                    .unwrap()
                    .get_ssh_pub_key(json!({ "tunnel_id": tunnel_id }))
                    .await
                    .unwrap()
                    .unwrap()
                    .to_string()
            });
            assert!(!response.starts_with(
                "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
"
            ));
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    // we need here multiple threads in order for the task spawned by
    // tokio::spawn to be processed.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_ssh_tunnel_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/cert.pub",
        ];
        let env_vars = vec![
            ("SUPPRESS_SYSTEM_INFO", "true"),
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            ("SUPPRESS_PROVISIONING_CONFIG", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .with(eq(json!({"ssh_tunnel":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report().times(8).returning(|_| Ok(()));

            // currently no way to explicitly wait for the spawned task
            // mock.expect_send_d2c_message().times(1).returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.connect_twin().await }).is_ok());

            let cert_path = test_attr.dir.join("cert.pub");

            // test empty tunnel id
            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<SshTunnel>()
                    .unwrap()
                    .open_ssh_tunnel(json!({
                        "tunnel_id": "",
                        "certificate": std::fs::read_to_string(&cert_path).unwrap(),
                        "host": "test-host",
                        "port": 2222,
                        "user": "test-user",
                        "socket_path": "/some/test/socket/path",
                    }))
                    .await
            })
            .is_err());

            // test non-uuid tunnel id
            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<SshTunnel>()
                    .unwrap()
                    .open_ssh_tunnel(json!({
                        "tunnel_id": "Don't panic!",
                        "certificate": std::fs::read_to_string(&cert_path).unwrap(),
                        "host": "test-host",
                        "port": 2222,
                        "user": "test-user",
                        "socket_path": "/some/test/socket/path",
                    }))
                    .await
            })
            .is_err());

            // test successful
            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<SshTunnel>()
                    .unwrap()
                    .open_ssh_tunnel(json!({
                        "tunnel_id": "b7afb216-5f7a-4755-a300-9374f8a0e9ff",
                        "certificate": std::fs::read_to_string(&cert_path).unwrap(),
                        "host": "test-host",
                        "port": 2222,
                        "user": "test-user",
                        "socket_path": "/some/test/socket/path",
                    }))
                    .await
            })
            .is_ok());

            // test connection limit
            let pipe_names = (1..=5)
                .into_iter()
                .map(|pipe_num| test_attr.dir.join(&format!("named_pipe_{}", pipe_num)))
                .collect::<Vec<_>>();

            for pipe_name in &pipe_names {
                Command::new("mkfifo").arg(pipe_name).output().unwrap();
            }

            // the first 5 requests should succeed
            for pipe_name in &pipe_names[0..=4] {
                assert!(block_on(async {
                    test_attr
                        .twin
                        .feature::<SshTunnel>()
                        .unwrap()
                        .open_ssh_tunnel(json!({
                            "tunnel_id": "b7afb216-5f7a-4755-a300-9374f8a0e9ff",
                            "certificate": std::fs::read_to_string(&cert_path).unwrap(),
                            "host": pipe_name,
                            "port": 2222,
                            "user": "test-user",
                            "socket_path": "/some/test/socket/path",
                        }))
                        .await
                })
                .is_ok());
            }

            // the final should fail
            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<SshTunnel>()
                    .unwrap()
                    .open_ssh_tunnel(json!({
                        "tunnel_id": "b7afb216-5f7a-4755-a300-9374f8a0e9ff",
                        "certificate": std::fs::read_to_string(&cert_path).unwrap(),
                        "host": "test-host",
                        "port": 2222,
                        "user": "test-user",
                        "socket_path": "/some/test/socket/path",
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
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[cfg(test)]
    mod test {
        use regex::Regex;

        #[test]
        fn test_utc_regex() {
            let time_string = "2023-09-12T23:59:00Z";

            let re = Regex::new(super::UTC_REGEX).unwrap();
            assert!(re.is_match(time_string));
        }

        #[test]
        fn test_utc_regex_with_sub_second_places() {
            let time_string = "2023-09-12T23:59:00.1234Z";

            let re = Regex::new(super::UTC_REGEX).unwrap();
            assert!(re.is_match(time_string));
        }

        #[test]
        fn test_utc_regex_with_many_sub_second_places() {
            let time_string = "2023-09-12T23:59:00.123456789123456789Z";

            let re = Regex::new(super::UTC_REGEX).unwrap();
            assert!(re.is_match(time_string));
        }

        #[test]
        fn test_utc_regex_with_positive_offset() {
            let time_string = "2023-09-12T23:59:00.1234+13:20";

            let re = Regex::new(super::UTC_REGEX).unwrap();
            assert!(re.is_match(time_string));
        }

        #[test]
        fn test_utc_regex_with_negative_offset() {
            let time_string = "2023-09-12T23:59:00.1234-13:20";

            let re = Regex::new(super::UTC_REGEX).unwrap();
            assert!(re.is_match(time_string));
        }
    }
}
