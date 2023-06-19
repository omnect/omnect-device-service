#[cfg(test)]
mod mod_test {
    use super::super::*;
    use crate::test_util::TestEnvironment;
    use crate::twin;
    use crate::{consent_path, history_consent_path};
    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    use regex::Regex;
    use serde_json::json;
    use std::env;
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::mpsc::{Receiver, TryRecvError};
    use std::time::Duration;

    struct TestCase;

    struct TestAttributes<'a> {
        twin: &'a mut Twin,
        rx: Receiver<Message>,
        dir: PathBuf,
    }

    impl TestCase {
        fn run<TestFn>(
            test_files: Vec<&str>,
            test_dirs: Vec<&str>,
            env_vars: Vec<(&str, &str)>,
            test: TestFn,
        ) where
            TestFn: Fn(&mut TestAttributes),
        {
            let (tx, rx) = mpsc::channel();
            let guard = twin::get_or_init(Some(&tx));
            let twin_lock = guard.inner.lock();
            let twin_lock = &mut *twin_lock.unwrap();

            if twin_lock.is_none() {
                *twin_lock = Some(Twin::new(tx.clone()));
            }

            let testcase_name: String = thread_rng()
                .sample_iter(&Alphanumeric)
                .take(10)
                .map(char::from)
                .collect();

            let test_env = TestEnvironment::new(testcase_name.as_str());

            let mut test_attr = TestAttributes {
                twin: twin_lock.as_mut().unwrap(),
                rx: rx,
                dir: PathBuf::from(test_env.dirpath()),
            };

            env::set_var("OS_RELEASE_DIR_PATH", test_env.dirpath().as_str());
            env::set_var("CONSENT_DIR_PATH", test_env.dirpath().as_str());
            test_files.iter().for_each(|file| {
                test_env.copy_file(file);
                ()
            });

            test_dirs.iter().for_each(|dir| {
                test_env.copy_directory(dir);
                ()
            });

            env_vars.iter().for_each(|env| env::set_var(env.0, env.1));

            test(&mut test_attr);

            env::remove_var("OS_RELEASE_DIR_PATH");
            env::remove_var("CONSENT_DIR_PATH");
            env_vars.iter().for_each(|e| env::remove_var(e.0));

            *twin_lock = None;
        }
    }

    fn recv_exact_num_msgs(num: u32, rx: &Receiver<Message>) -> Vec<serde_json::Value> {
        let mut reported_results = vec![];

        for n in 0..num {
            match rx.try_recv() {
                Ok(Message::Reported(v)) => reported_results.push(v),
                Err(mpsc::TryRecvError::Disconnected) => panic!("unexpected disconnect"),
                Err(mpsc::TryRecvError::Empty) => {
                    panic!("unexpected number of messages: {n}/{num}")
                }
                _ => panic!("unexpected message"),
            }
        }

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        reported_results
    }

    #[tokio::test]
    async fn init_features_ok_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            let reported_results =
                recv_exact_num_msgs((TwinFeature::COUNT + 8).try_into().unwrap(), &test_attr.rx);

            assert_eq!(
                reported_results.first().unwrap(),
                &json!({
                    "module-version": env!("CARGO_PKG_VERSION"),
                    "azure-sdk-version": IotHubClient::sdk_version_string()
                })
            );

            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"factory_reset":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"ssh":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"device_update_consent":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"network_status":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"reboot":{"version":1}})));
            assert!(!reported_results
                .iter()
                .any(|f| f == &json!({"wifi_commissioning":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"device_update_consent":{"general_consent":["swupdate"]}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"device_update_consent":{"user_consent_request":[{"swupdate":"<version>"}]}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"device_update_consent":{"user_consent_history":{"swupdate":["<version>"]}}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"ssh":{"status":{"v4_enabled":false,"v6_enabled":false}}})));
            assert!(reported_results.iter().any(|f| {
                let reported = format!("{:?}", f);
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
                re.is_match(reported)
            }));
        };

        TestCase::run(test_files, vec![], vec![], test);
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

        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            let reported_results =
                recv_exact_num_msgs((TwinFeature::COUNT + 7).try_into().unwrap(), &test_attr.rx);

            assert_eq!(
                reported_results.first().unwrap(),
                &json!({
                    "module-version": env!("CARGO_PKG_VERSION"),
                    "azure-sdk-version": IotHubClient::sdk_version_string()
                })
            );
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({ "factory_reset": null })));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"ssh":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"device_update_consent":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"network_status":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"reboot":{"version":1}})));
            assert!(!reported_results
                .iter()
                .any(|f| f == &json!({"wifi_commissioning":{"version":1}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"device_update_consent":{"general_consent":["swupdate"]}})));
            assert!(reported_results
                    .iter()
                    .any(|f| f == &json!({"device_update_consent":{"user_consent_request":[{"swupdate":"<version>"}]}})));
            assert!(reported_results
                    .iter()
                    .any(|f| f == &json!({"device_update_consent":{"user_consent_history":{"swupdate":["<version>"]}}})));
            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"ssh":{"status":{"v4_enabled":false,"v6_enabled":false}}})));

            assert!(test_attr.twin.feature::<FactoryReset>().is_err());
        };

        TestCase::run(test_files, vec![], env_vars, test);
    }

    #[test]
    fn wifi_commissioning_feature_test() {
        let test_files = vec!["testfiles/wifi_commissioning/os-release"];
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_FACTORY_RESET", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
            ("SUPPRESS_REBOOT", "true"),
        ];
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            let reported_results = recv_exact_num_msgs(8, &test_attr.rx);

            assert!(reported_results
                .iter()
                .any(|f| f == &json!({"wifi_commissioning":{"version":1}})));
        };

        TestCase::run(test_files, vec![], env_vars, test);
    }

    #[tokio::test]
    async fn update_twin_test() {
        let env_vars = vec![
            ("SUPPRESS_DEVICE_UPDATE_USER_CONSENT", "true"),
            ("SUPPRESS_NETWORK_STATUS", "true"),
        ];

        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr
                .twin
                .update(TwinUpdateState::Partial, json!(""))
                .is_ok());
            assert_eq!(
                test_attr
                    .twin
                    .update(TwinUpdateState::Partial, json!({"general_consent": {}}))
                    .unwrap_err()
                    .to_string(),
                "feature disabled: device_update_consent"
            );
            assert_eq!(
                test_attr
                    .twin
                    .update(
                        TwinUpdateState::Partial,
                        json!({"include_network_filter": []})
                    )
                    .unwrap_err()
                    .to_string(),
                "feature disabled: network_status"
            );

            assert_eq!(
                test_attr
                    .twin
                    .update(TwinUpdateState::Complete, json!(""))
                    .unwrap_err()
                    .to_string(),
                "update: 'desired' missing while TwinUpdateState::Complete"
            );
            assert_eq!(
                test_attr
                    .twin
                    .update(
                        TwinUpdateState::Complete,
                        json!({"desired": {"general_consent": {}}})
                    )
                    .unwrap_err()
                    .to_string(),
                "feature disabled: device_update_consent"
            );
        };

        TestCase::run(vec![], vec![], env_vars, test);
    }

    #[tokio::test]
    async fn update_and_report_general_consent_failed_test1() {
        let test = |test_attr: &mut TestAttributes| {
            let usr_consent = test_attr.twin.feature::<DeviceUpdateConsent>();

            assert!(usr_consent.is_ok());

            let usr_consent = usr_consent.unwrap();
            let err = usr_consent.update_general_consent(None).unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("update_general_consent: report_general_consent")));

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("report_general_consent: open consent_conf.json")));

            recv_exact_num_msgs(0, &test_attr.rx);

            let err = usr_consent
                .update_general_consent(Some(json!([1, 1]).as_array().unwrap()))
                .unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("update_general_consent: parse desired_consents")));

            recv_exact_num_msgs(0, &test_attr.rx);

            let err = usr_consent
                .update_general_consent(Some(json!(["1", "1"]).as_array().unwrap()))
                .unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("update_general_consent: open consent_conf.json")));

            recv_exact_num_msgs(0, &test_attr.rx);
        };

        TestCase::run(vec![], vec![], vec![], test);
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

        let test = |test_attr: &mut TestAttributes| {
            let err = test_attr.twin.init_features().unwrap_err();

            assert!(err.chain().any(|e| e
                .to_string()
                .starts_with("report_user_consent: serde_json::from_reader")));

            recv_exact_num_msgs(9, &test_attr.rx);
        };

        TestCase::run(test_files, vec![], env_vars, test);
    }

    #[tokio::test]
    async fn update_and_report_general_consent_passed_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs((TwinFeature::COUNT + 8).try_into().unwrap(), &test_attr.rx);

            assert!(test_attr
                .twin
                .update(TwinUpdateState::Complete, json!({"desired": {}}))
                .is_ok());

            let reported_results = recv_exact_num_msgs(1, &test_attr.rx);

            assert_eq!(
                reported_results.first().unwrap(),
                &json!({
                    "device_update_consent": {
                        "general_consent": ["swupdate"]
                    }
                })
            );

            assert!(test_attr
                .twin
                .update(
                    TwinUpdateState::Partial,
                    json!({"general_consent": ["SWUPDATE2", "SWUPDATE1"]})
                )
                .is_ok());

            let reported_results = recv_exact_num_msgs(1, &test_attr.rx);

            assert_eq!(
                reported_results.first().unwrap(),
                &json!({
                    "device_update_consent": {
                        "general_consent": ["swupdate1", "swupdate2"]
                    }
                })
            );

            assert!(test_attr
                .twin
                .update(TwinUpdateState::Complete, json!({"desired": {}}))
                .is_ok());

            let reported_results = recv_exact_num_msgs(1, &test_attr.rx);

            assert_eq!(
                reported_results.first().unwrap(),
                &json!({
                    "device_update_consent": {
                        "general_consent": ["swupdate1", "swupdate2"]
                    }
                })
            );
        };

        TestCase::run(test_files, vec![], vec![], test);
    }

    #[tokio::test]
    async fn consent_thread_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs((TwinFeature::COUNT + 8).try_into().unwrap(), &test_attr.rx);

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

            let reported_results = recv_exact_num_msgs(1, &test_attr.rx);

            assert_eq!(
                reported_results.first().unwrap(),
                &json!({
                    "device_update_consent": {
                        "user_consent_history": {
                            "swupdate": [
                                "1.2.3"
                            ]
                        }
                    }
                })
            );
        };

        TestCase::run(test_files, vec![], vec![], test);
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

        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs((TwinFeature::COUNT + 8).try_into().unwrap(), &test_attr.rx);

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

        TestCase::run(test_files, test_dirs, vec![], test);
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

        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs((TwinFeature::COUNT + 8).try_into().unwrap(), &test_attr.rx);

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

        TestCase::run(test_files, test_dirs, vec![], test);
    }

    #[test]
    fn reset_to_factory_settings_test() {
        let test_files = vec![
            "testfiles/positive/os-release",
            "testfiles/positive/consent_conf.json",
            "testfiles/positive/request_consent.json",
            "testfiles/positive/history_consent.json",
        ];

        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs((TwinFeature::COUNT + 8).try_into().unwrap(), &test_attr.rx);

            let factory_reset = test_attr.twin.feature::<FactoryReset>().unwrap();

            assert_eq!(
                factory_reset
                    .reset_to_factory_settings(json!({
                        "restore_settings": ["wifi"]
                    }),)
                    .unwrap_err()
                    .to_string(),
                "reset type missing or not supported"
            );

            assert_eq!(
                factory_reset
                    .reset_to_factory_settings(json!({
                        "type": 1,
                        "restore_settings": ["unknown"]
                    }),)
                    .unwrap_err()
                    .to_string(),
                "unknown restore setting received"
            );

            assert!(factory_reset
                .reset_to_factory_settings(json!({
                    "type": 1,
                    "restore_settings": ["wifi"]
                }),)
                .is_ok());

            let reported_results = recv_exact_num_msgs(1, &test_attr.rx);
            let reported = format!("{:?}", reported_results.first().unwrap());

            let re = format!(
                "{}{}{}",
                regex::escape(
                    r#"Object {"factory_reset": Object {"status": Object {"date": String(""#,
                ),
                r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                regex::escape(r#"Z"), "status": String("in_progress")}}}"#,),
            );
            let re = Regex::new(re.as_str()).unwrap();

            assert!(re.is_match(reported.as_str()));
        };

        TestCase::run(test_files, vec![], vec![], test);
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
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs(8, &test_attr.rx);

            assert_eq!(
                test_attr
                    .twin
                    .feature::<FactoryReset>()
                    .unwrap()
                    .reset_to_factory_settings(json!({
                        "type": 1,
                        "restore_settings": ["wifi"]
                    }))
                    .unwrap(),
                None
            );

            let reported_results = recv_exact_num_msgs(1, &test_attr.rx);
            let reported = format!("{:?}", reported_results.first().unwrap());

            let re = format!(
                "{}{}{}",
                regex::escape(
                    r#"Object {"factory_reset": Object {"status": Object {"date": String(""#,
                ),
                r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                regex::escape(r#"Z"), "status": String("in_progress")}}}"#,),
            );
            let re = Regex::new(re.as_str()).unwrap();

            assert!(re.is_match(reported.as_str()));
        };

        TestCase::run(test_files, vec![], env_vars, test);
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
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs(7, &test_attr.rx);
        };

        TestCase::run(test_files, vec![], env_vars, test);
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
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs(7, &test_attr.rx);
        };

        TestCase::run(test_files, vec![], env_vars, test);
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
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            let reported_results = recv_exact_num_msgs(8, &test_attr.rx);
            assert!(reported_results.iter().any(|f| {
                let reported = format!("{:?}", f);
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
                re.is_match(reported)
            }));
        };

        TestCase::run(test_files, vec![], env_vars, test);
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
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            let reported_results = recv_exact_num_msgs(8, &test_attr.rx);
            assert!(reported_results.iter().any(|f| {
                let reported = format!("{:?}", f);
                let reported = reported.as_str();

                let re = format!(
                    "{}{}{}",
                    regex::escape(
                        r#"Object {"factory_reset": Object {"status": Object {"date": String(""#,
                    ),
                    r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{8,9}",
                    regex::escape(r#"Z"), "status": String("unexpected factory reset type")}}}"#,),
                );

                let re = Regex::new(re.as_str()).unwrap();
                re.is_match(reported)
            }));
        };

        TestCase::run(test_files, vec![], env_vars, test);
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
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs(7, &test_attr.rx);

            assert!(test_attr
                .twin
                .update(
                    TwinUpdateState::Partial,
                    json!({"include_network_filter": []}),
                )
                .is_ok());

            let reported_results = recv_exact_num_msgs(1, &test_attr.rx);

            assert_eq!(
                reported_results.first().unwrap(),
                &json!({
                    "network_status": {
                        "interfaces": []
                    }
                })
            );

            assert!(test_attr
                .twin
                .update(
                    TwinUpdateState::Partial,
                    json!({"include_network_filter": []}),
                )
                .is_ok());

            recv_exact_num_msgs(0, &test_attr.rx);

            assert!(test_attr
                .twin
                .update(
                    TwinUpdateState::Partial,
                    json!({ "include_network_filter": ["*"] }),
                )
                .is_ok());

            recv_exact_num_msgs(1, &test_attr.rx);

            assert!(test_attr
                .twin
                .update(
                    TwinUpdateState::Partial,
                    json!({ "include_network_filter": ["*"] }),
                )
                .is_ok());

            recv_exact_num_msgs(0, &test_attr.rx);

            assert!(test_attr
                .twin
                .feature::<NetworkStatus>()
                .unwrap()
                .refresh_network_status()
                .is_ok());

            recv_exact_num_msgs(1, &test_attr.rx);
        };

        TestCase::run(test_files, vec![], env_vars, test);
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
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs(8, &test_attr.rx);

            assert!(test_attr
                .twin
                .feature::<Ssh>()
                .unwrap()
                .open_ssh(json!({ "pubkey": "" }),)
                .unwrap_err()
                .to_string()
                .starts_with("Empty ssh pubkey"));

            assert!(test_attr
                .twin
                .feature::<Ssh>()
                .unwrap()
                .open_ssh(json!({}),)
                .unwrap_err()
                .to_string()
                .starts_with("No ssh pubkey given"));

            assert!(test_attr
                .twin
                .feature::<Ssh>()
                .unwrap()
                .open_ssh(json!({ "": "" }),)
                .unwrap_err()
                .to_string()
                .starts_with("No ssh pubkey given"));

            assert!(test_attr
                .twin
                .feature::<Ssh>()
                .unwrap()
                .open_ssh(json!({ "pubkey": "mykey" }))
                .is_err());

            // its hard to test the ssh functionality as module test,
            // we would have to mock the iptables crate and make
            // AUTHORIZED_KEY_PATH configurable
        };

        TestCase::run(test_files, vec![], env_vars, test);
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
        let test = |test_attr: &mut TestAttributes| {
            assert!(test_attr.twin.init_features().is_ok());

            recv_exact_num_msgs(8, &test_attr.rx);

            assert!(test_attr
                .twin
                .feature::<Ssh>()
                .unwrap()
                .refresh_ssh_status()
                .is_ok());

            let reported_results = recv_exact_num_msgs(1, &test_attr.rx);

            assert_eq!(
                reported_results.first().unwrap(),
                &json!({"ssh": {
                    "status": {
                        "v4_enabled":false,
                        "v6_enabled":false
                    }
                }})
            );
        };

        TestCase::run(test_files, vec![], env_vars, test);
    }
}
