#[cfg(test)]
mod mod_test {
    use super::super::*;
    use crate::test_util::mod_test::TestEnvironment;
    use crate::{consent_path, history_consent_path};
    use mockall::{mock, predicate::*};
    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    use regex::Regex;
    use serde_json::json;
    use std::env;
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use std::time::Duration;

    mock! {
        MyIotHub {}

        #[async_trait(?Send)]
        impl IotHub for MyIotHub {
            fn sdk_version_string() -> String;
            fn client_type() -> ClientType;
            fn from_edge_environment(
                tx_connection_status: Option<mpsc::Sender<AuthenticationStatus>>,
                tx_twin_desired: Option<mpsc::Sender<(TwinUpdateState, serde_json::Value)>>,
                tx_direct_method: Option<DirectMethodSender>,
                tx_incoming_message: Option<IncomingMessageObserver>,
            ) -> Result<Box<Self>>;
            fn from_identity_service(
                _tx_connection_status: Option<mpsc::Sender<AuthenticationStatus>>,
                _tx_twin_desired: Option<mpsc::Sender<(TwinUpdateState, serde_json::Value)>>,
                _tx_direct_method: Option<DirectMethodSender>,
                _tx_incoming_message: Option<IncomingMessageObserver>,
            ) -> Result<Box<Self>>;
            fn from_connection_string(
                connection_string: &str,
                tx_connection_status: Option<mpsc::Sender<AuthenticationStatus>>,
                tx_twin_desired: Option<mpsc::Sender<(TwinUpdateState, serde_json::Value)>>,
                tx_direct_method: Option<DirectMethodSender>,
                tx_incoming_message: Option<IncomingMessageObserver>,
            ) -> Result<Box<Self>>;
            fn twin_async(&mut self) -> Result<()>;
            async fn send_d2c_message(&mut self, mut message: IotMessage) -> Result<()>;
            async fn twin_report(&mut self, reported: serde_json::Value) -> Result<()>;
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
            test_files.iter().for_each(|file| {
                test_env.copy_file(file);
                ()
            });

            test_dirs.iter().for_each(|dir| {
                test_env.copy_directory(dir);
                ()
            });

            // set env vars
            env::set_var("OS_RELEASE_DIR_PATH", test_env.dirpath().as_str());
            env::set_var("CONSENT_DIR_PATH", test_env.dirpath().as_str());

            env_vars.iter().for_each(|env| env::set_var(env.0, env.1));
            let (tx_reported_properties, mut rx_reported_properties) = mpsc::channel(100);

            // create iothub client mock
            let ctx = MockMyIotHub::from_connection_string_context();
            ctx.expect().returning(|_, _, _, _, _| {
                let mock = MockMyIotHub::default();
                Ok(Box::new(mock))
            });

            let mut mock =
                MockMyIotHub::from_connection_string("", None, None, None, None).unwrap();

            // set testcase specific mock expectaions
            set_mock_expectations(&mut mock);

            // create test config
            let mut config = TestConfig {
                twin: Twin::new(mock, tx_reported_properties),
                dir: PathBuf::from(test_env.dirpath()),
            };

            // run test
            run_test(&mut config);

            // compute reported properties
            loop {
                match rx_reported_properties.try_recv() {
                    Ok(val) => {
                        block_on(async { config.twin.handle_report_property(val).await }).unwrap()
                    }
                    _ => break,
                }
            }

            // cleanup env vars
            env::remove_var("OS_RELEASE_DIR_PATH");
            env::remove_var("CONSENT_DIR_PATH");
            env_vars.iter().for_each(|e| env::remove_var(e.0));
        }
    }

    #[tokio::test]
    async fn init_features_ok_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .with(eq(json!({
                    "module-version": env!("CARGO_PKG_VERSION"),
                    "azure-sdk-version": IotHubClient::sdk_version_string()
                })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"factory_reset":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"ssh":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"device_update_consent":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"network_status":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"reboot":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({ "wifi_commissioning": null })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(
                    json!({"device_update_consent":{"general_consent":["swupdate"]}}),
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
                .with(eq(
                    json!({"ssh":{"status":{"v4_enabled":false,"v6_enabled":false}}}),
                ))
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
                        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                        regex::escape(r#"Z"), "status": String("succeeded")}}}"#,),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &'_ mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());
        };

        TestCase::run(test_files, vec![], vec![], expect, test);
    }

    #[tokio::test]
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
                .with(eq(json!({
                    "module-version": env!("CARGO_PKG_VERSION"),
                    "azure-sdk-version": IotHubClient::sdk_version_string()
                })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({ "factory_reset": null })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"ssh":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"device_update_consent":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"network_status":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({"reboot":{"version":1}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({ "wifi_commissioning": null })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(
                    json!({"device_update_consent":{"general_consent":["swupdate"]}}),
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
                .with(eq(
                    json!({"ssh":{"status":{"v4_enabled":false,"v6_enabled":false}}}),
                ))
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &'_ mut TestConfig| {
            assert!(test_attr.twin.feature::<FactoryReset>().is_err());
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[test]
    fn wifi_commissioning_feature_test() {
        let test_files = vec!["testfiles/wifi_commissioning/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_SSH_HANDLING", "true"),
            ("SUPPRESS_REBOOT", "true"),
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
            assert!(test_attr.twin.feature::<Ssh>().is_err());
            assert!(test_attr.twin.feature::<Reboot>().is_err());
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[tokio::test]
    async fn update_twin_test() {
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
        ];

        let expect = |_mock: &mut MockMyIotHub| {};

        let test = |test_attr: &'_ mut TestConfig| {
            assert!(block_on(async {
                test_attr
                    .twin
                    .update(TwinUpdateState::Partial, json!(""))
                    .await
            })
            .is_ok());

            assert_eq!(
                block_on(async {
                    test_attr
                        .twin
                        .update(TwinUpdateState::Partial, json!({"general_consent": {}}))
                        .await
                })
                .unwrap_err()
                .to_string(),
                "feature disabled: device_update_consent"
            );

            assert_eq!(
                block_on(async {
                    test_attr
                        .twin
                        .update(
                            TwinUpdateState::Partial,
                            json!({"include_network_filter": []}),
                        )
                        .await
                })
                .unwrap_err()
                .to_string(),
                "feature disabled: network_status"
            );

            assert_eq!(
                block_on(async {
                    test_attr
                        .twin
                        .update(TwinUpdateState::Complete, json!(""))
                        .await
                })
                .unwrap_err()
                .to_string(),
                "update: 'desired' missing while TwinUpdateState::Complete"
            );
            assert_eq!(
                block_on(async {
                    test_attr
                        .twin
                        .update(
                            TwinUpdateState::Complete,
                            json!({"desired": {"general_consent": {}}}),
                        )
                        .await
                })
                .unwrap_err()
                .to_string(),
                "feature disabled: device_update_consent"
            );
        };

        TestCase::run(vec![], vec![], env_vars, expect, test);
    }

    #[tokio::test]
    async fn update_and_report_general_consent_failed_test1() {
        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report().times(0).returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            let usr_consent = test_attr.twin.feature::<DeviceUpdateConsent>();

            assert!(usr_consent.is_ok());

            let usr_consent = usr_consent.unwrap();
            let err =
                block_on(async { usr_consent.update_general_consent(None).await }).unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("update_general_consent: report_general_consent")));

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("report_general_consent: open consent_conf.json")));

            let err = block_on(async {
                usr_consent
                    .update_general_consent(Some(json!([1, 1]).as_array().unwrap()))
                    .await
            })
            .unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("update_general_consent: parse desired_consents")));

            let err = block_on(async {
                usr_consent
                    .update_general_consent(Some(json!(["1", "1"]).as_array().unwrap()))
                    .await
            })
            .unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("update_general_consent: open consent_conf.json")));
        };

        TestCase::run(vec![], vec![], vec![], expect, test);
    }

    #[tokio::test]
    async fn update_and_report_general_consent_failed_test2() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/negative/history_consent.json",
        ];
        let env_vars = vec![
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_SSH_HANDLING", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report().times(9).returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            let err = block_on(async { test_attr.twin.init_features().await }).unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("report_user_consent: serde_json::from_reader")));
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[tokio::test]
    async fn update_and_report_general_consent_passed_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 8)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({
                    "device_update_consent": {
                        "general_consent": ["swupdate"]
                    }
                })))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report()
                .with(eq(json!({
                    "device_update_consent": {
                        "general_consent": ["swupdate1", "swupdate2"]
                    }
                })))
                .times(2)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .update(TwinUpdateState::Complete, json!({"desired": {}}))
                    .await
            })
            .is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .update(
                        TwinUpdateState::Partial,
                        json!({"general_consent": ["SWUPDATE2", "SWUPDATE1"]}),
                    )
                    .await
            })
            .is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .update(TwinUpdateState::Complete, json!({"desired": {}}))
                    .await
            })
            .is_ok());
        };

        TestCase::run(test_files, vec![], vec![], expect, test);
    }

    #[tokio::test]
    async fn consent_observe_history_file_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 8)
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
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

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

    #[tokio::test]
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
                .times(TwinFeature::COUNT + 8)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

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

    #[tokio::test]
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
                .times(TwinFeature::COUNT + 8)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

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

    #[test]
    fn reset_to_factory_settings_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 8)
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
                        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                        regex::escape(r#"Z"), "status": String("in_progress")}}}"#,),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

            let factory_reset = test_attr.twin.feature::<FactoryReset>().unwrap();

            assert_eq!(
                block_on(async {
                    factory_reset
                        .reset_to_factory_settings(json!({
                            "restore_settings": ["wifi"]
                        }))
                        .await
                })
                .unwrap_err()
                .to_string(),
                "reset type missing or not supported"
            );

            assert_eq!(
                block_on(async {
                    factory_reset
                        .reset_to_factory_settings(json!({
                            "type": 1,
                            "restore_settings": ["unknown"]
                        }))
                        .await
                })
                .unwrap_err()
                .to_string(),
                "unknown restore setting received"
            );

            assert!(block_on(async {
                factory_reset
                    .reset_to_factory_settings(json!({
                        "type": 1,
                        "restore_settings": ["wifi"]
                    }))
                    .await
            })
            .is_ok());
        };

        TestCase::run(test_files, vec![], vec![], expect, test);
    }

    #[tokio::test]
    async fn factory_reset_direct_method_test() {
        let test_files = vec!["testfiles/positive/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_SSH_HANDLING", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 3)
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
                        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                        regex::escape(r#"Z"), "status": String("succeeded")}}}"#,),
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
                        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                        regex::escape(r#"Z"), "status": String("in_progress")}}}"#,),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

            let factory_reset = test_attr.twin.feature::<FactoryReset>().unwrap();

            assert_eq!(
                block_on(async {
                    factory_reset
                        .reset_to_factory_settings(json!({
                            "type": 1,
                            "restore_settings": ["wifi"]
                        }))
                        .await
                })
                .unwrap(),
                None
            );
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[test]
    fn factory_reset_unexpected_result_test() {
        let test_files = vec!["testfiles/positive/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_SSH_HANDLING", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            (
                "TEST_FACTORY_RESET_RESULT",
                "unexpected_factory_reset_result_format",
            ),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 3)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[test]
    fn factory_reset_normal_result_test() {
        let test_files = vec!["testfiles/positive/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_SSH_HANDLING", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            (
                "TEST_FACTORY_RESET_RESULT",
                "normal_boot_without_factory_reset",
            ),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 3)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[test]
    fn factory_reset_unexpected_setting_test() {
        let test_files = vec!["testfiles/positive/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_SSH_HANDLING", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            (
                "TEST_FACTORY_RESET_RESULT",
                "unexpected_restore_settings_error",
            ),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 3)
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
                        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                        regex::escape(
                            r#"Z"), "status": String("unexpected restore settings error")}}}"#,
                        ),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[test]
    fn factory_reset_unexpected_type_test() {
        let test_files = vec!["testfiles/positive/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_SSH_HANDLING", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
            ("TEST_FACTORY_RESET_RESULT", "unexpected_factory_reset_type"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .times(TwinFeature::COUNT + 3)
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
                        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                        regex::escape(
                            r#"Z"), "status": String("unexpected factory reset type")}}}"#,
                        ),
                    );

                    let re = Regex::new(re.as_str()).unwrap();
                    re.is_match(reported)})
                .times(1)
                .returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[test]
    fn update_report_and_refresh_network_status_test() {
        let test_files = vec!["testfiles/positive/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_SSH_HANDLING", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_REBOOT", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .with(eq(json!({"network_status":{"interfaces":[]}})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report().times(9).returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .update(
                        TwinUpdateState::Partial,
                        json!({"include_network_filter": []}),
                    )
                    .await
            })
            .is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .update(
                        TwinUpdateState::Partial,
                        json!({"include_network_filter": []}),
                    )
                    .await
            })
            .is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .update(
                        TwinUpdateState::Partial,
                        json!({ "include_network_filter": ["*"] }),
                    )
                    .await
            })
            .is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .update(
                        TwinUpdateState::Partial,
                        json!({ "include_network_filter": ["*"] }),
                    )
                    .await
            })
            .is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<NetworkStatus>()
                    .unwrap()
                    .refresh_network_status()
                    .await
            })
            .is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[test]
    fn open_ssh_test() {
        let test_files = vec!["testfiles/positive/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report().times(8).returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<Ssh>()
                    .unwrap()
                    .open_ssh(json!({ "pubkey": "" }))
                    .await
            })
            .unwrap_err()
            .to_string()
            .starts_with("Empty ssh pubkey"));

            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<Ssh>()
                    .unwrap()
                    .open_ssh(json!({}))
                    .await
            })
            .unwrap_err()
            .to_string()
            .starts_with("No ssh pubkey given"));

            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<Ssh>()
                    .unwrap()
                    .open_ssh(json!({ "": "" }))
                    .await
            })
            .unwrap_err()
            .to_string()
            .starts_with("No ssh pubkey given"));

            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<Ssh>()
                    .unwrap()
                    .open_ssh(json!({ "pubkey": "mykey" }))
                    .await
            })
            .is_err());

            // its hard to test the ssh functionality as module test,
            // we would have to mock the iptables crate and make
            // AUTHORIZED_KEY_PATH configurable
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }

    #[test]
    fn refresh_ssh_test() {
        let test_files = vec!["testfiles/positive/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
        ];

        let expect = |mock: &mut MockMyIotHub| {
            mock.expect_twin_report()
                .with(eq(json!({"ssh": {
                            "status": {
                                "v4_enabled":false,
                                "v6_enabled":false
                            }
                }})))
                .times(1)
                .returning(|_| Ok(()));

            mock.expect_twin_report().times(8).returning(|_| Ok(()));
        };

        let test = |test_attr: &mut TestConfig| {
            assert!(block_on(async { test_attr.twin.init_features().await }).is_ok());

            assert!(block_on(async {
                test_attr
                    .twin
                    .feature::<Ssh>()
                    .unwrap()
                    .refresh_ssh_status()
                    .await
            })
            .is_ok());
        };

        TestCase::run(test_files, vec![], env_vars, expect, test);
    }
}
