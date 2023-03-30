#[cfg(test)]
mod mod_test {
    use super::super::*;
    use crate::test_util::Testrunner;
    use lazy_static::lazy_static;
    use serde_json::json;
    use std::env;
    use std::sync::mpsc::TryRecvError;
    use std::sync::{mpsc, Mutex};
    use stdext::function_name;

    /* ToDo:
        as soon as we can switch to rust >=1.63 we should replace by:
        `static TESTCASE: Mutex<i32> = Mutex::new(0i32);`
    */
    lazy_static! {
        static ref TESTCASE: Mutex<i32> = Mutex::new(0i32);
    }

    #[test]
    fn report_impl_test() {
        let mut twin = Twin {
            tx: None,
            include_network_filter: None,
        };

        assert!(twin
            .report_impl(json!({"test": "test"}))
            .unwrap_err()
            .to_string()
            .starts_with("tx channel missing"));

        let (tx, rx) = mpsc::channel();
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin.report_impl(json!({"test": "test"})).is_ok());
        assert_eq!(
            rx.try_recv(),
            Ok(Message::Reported(json!({"test": "test"})))
        );
    }

    #[test]
    fn report_versions_test() {
        let (tx, rx) = mpsc::channel();
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin.report_versions().is_ok());

        assert_eq!(
            rx.try_recv(),
            Ok(Message::Reported(json!({
                "module-version": format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("GIT_SHORT_REV")),
                "azure-sdk-version": IotHubClient::get_sdk_version_string()
            })))
        );
    }

    #[test]
    fn update_and_report_general_consent_test() {
        // @ToDo: replace all `update_general_consent` by `update`calls
        let (tx, rx) = mpsc::channel();
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        let err = twin.update_general_consent(None).unwrap_err();

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("update_general_consent: report_general_consent")));

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("report_general_consent: open consent_conf.json")));

        let err = twin
            .update_general_consent(Some(json!([1, 1]).as_array().unwrap()))
            .unwrap_err();

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("update_general_consent: parse desired_consents")));

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        let err = twin
            .update_general_consent(Some(json!(["1", "1"]).as_array().unwrap()))
            .unwrap_err();

        assert!(err.chain().any(|e| e
            .to_string()
            .starts_with("update_general_consent: open consent_conf.json")));

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        // avoid race regarding CONSENT_DIR_PATH
        {
            let _lock = TESTCASE.lock().unwrap();

            let tr = Testrunner::new(function_name!().split("::").last().unwrap());
            tr.copy_file("testfiles/consent_conf.json");

            env::set_var("CONSENT_DIR_PATH", tr.get_dirpath().as_str());

            assert!(twin.update_general_consent(None).is_ok());

            assert_eq!(
                rx.try_recv(),
                Ok(Message::Reported(json!({"general_consent": ["swupdate"]})))
            );

            assert!(twin
                .update_general_consent(Some(json!(["SWUPDATE2", "SWUPDATE1"]).as_array().unwrap()))
                .is_ok());

            assert_eq!(
                rx.try_recv(),
                Ok(Message::Reported(
                    json!({"general_consent": ["swupdate1", "swupdate2"]})
                ))
            );

            assert!(twin.update_general_consent(None).is_ok());

            assert_eq!(
                rx.try_recv(),
                Ok(Message::Reported(
                    json!({"general_consent": ["swupdate1", "swupdate2"]})
                ))
            );
        }
    }

    #[test]
    fn report_user_consent_test() {
        let (tx, rx) = mpsc::channel();
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin
            .report_user_consent("invalid_path")
            .unwrap_err()
            .to_string()
            .starts_with("report_user_consent: open report_consent_file fo read"));

        let tr = Testrunner::new(function_name!().split("::").last().unwrap());

        assert!(twin
            .report_user_consent(
                tr.copy_file("testfiles/invalid_consent.json")
                    .to_str()
                    .unwrap()
            )
            .unwrap_err()
            .to_string()
            .starts_with("report_user_consent: serde_json::from_reader"));

        assert!(twin
            .report_user_consent(
                tr.copy_file("testfiles/history_consent.json")
                    .to_str()
                    .unwrap()
            )
            .is_ok());

        assert_eq!(
            rx.try_recv(),
            Ok(Message::Reported(json!({
                "user_consent_history": {
                    "swupdate": [
                        "<version>"
                    ]
                }
            })))
        );
    }

    // ToDo: we can not test this without mocking the reboot function
    #[ignore]
    #[test]
    fn report_factory_reset_status_test() {
        let (tx, rx) = mpsc::channel();
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin
            .report(&ReportProperty::FactoryResetStatus("in_progress"))
            .is_ok());

        let reported = rx.try_recv();

        assert!(reported.is_ok());
        let reported = format!("{:?}", reported.unwrap());

        assert!(reported
            .contains("Reported(Object {\"factory_reset_status\": Object {\"date\": String"));
        assert!(reported.contains("\"status\": String(\"in_progress\")}})"));
    }

    #[ignore]
    #[test]
    fn report_factory_reset_result_test() {
        let (tx, rx) = mpsc::channel();
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin.report_factory_reset_result().is_ok());

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        // ToDo: refactor report_factory_reset_result() for better testability
        // don't call "sudo fw_printenv factory-reset-status" command in function
        // but inject command output
    }

    #[test]
    fn update_and_report_network_status_test() {
        // @ToDo: replace all `update_include_network_filter` by `update`calls
        let (tx, rx) = mpsc::channel();
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin.report_network_status().is_ok());

        assert_eq!(
            rx.try_recv(),
            Ok(Message::Reported(json!({
                "network_interfaces": json!(null)
            })))
        );

        assert!(twin.update_include_network_filter(None).is_ok());

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        assert!(twin
            .update_include_network_filter(Some(json!(["*"]).as_array().unwrap()))
            .is_ok());

        assert!(rx.try_recv().is_ok());

        assert!(twin
            .update_include_network_filter(Some(json!(["*"]).as_array().unwrap()))
            .is_ok());

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        twin.include_network_filter = Some(vec!["*".to_string()]);

        assert!(twin.update_include_network_filter(None).is_ok());

        assert!(twin.report_network_status().is_ok());

        assert!(rx.try_recv().is_ok());

        assert_eq!(
            rx.try_recv(),
            Ok(Message::Reported(json!({
                "network_interfaces": json!(null)
            })))
        );

        // ToDo: add more tests based on network adapters available on build server?
    }

    #[test]
    fn factory_reset_test() {
        assert_eq!(
            factory_reset::reset_to_factory_settings(json!({
                "restore_settings": ["wifi"]
            }),)
            .unwrap_err()
            .to_string(),
            "reset type missing or not supported"
        );

        assert_eq!(
            factory_reset::reset_to_factory_settings(json!({
                "type": 1,
                "restore_settings": ["unknown"]
            }),)
            .unwrap_err()
            .to_string(),
            "unknown restore setting received"
        );
    }

    #[test]
    fn user_consent_test() {
        assert!(consent::user_consent(json!({"swupdate": "1.0.0"}))
            .unwrap_err()
            .to_string()
            .starts_with("user_consent: open user_consent.json for write"));

        assert_eq!(
            consent::user_consent(json!({})).unwrap_err().to_string(),
            "unexpected parameter format"
        );

        assert_eq!(
            consent::user_consent(json!({"swupdate": "1.0.0", "another_component": "1.2.3"}))
                .unwrap_err()
                .to_string(),
            "unexpected parameter format"
        );

        // avoid race regarding CONSENT_DIR_PATH
        {
            let _lock = TESTCASE.lock().unwrap();

            let tr = Testrunner::new(function_name!().split("::").last().unwrap());

            tr.copy_directory("testfiles/test_component");

            env::set_var("CONSENT_DIR_PATH", tr.get_dirpath().as_str());

            assert!(consent::user_consent(json!({"test_component": "1.0.0"})).is_ok());
        }
    }

    #[test]
    fn ssh_test() {
        assert!(ssh::open_ssh(json!({}),)
            .unwrap_err()
            .to_string()
            .starts_with("No ssh pubkey given"));
        assert!(ssh::open_ssh(json!({ "": "" }),)
            .unwrap_err()
            .to_string()
            .starts_with("No ssh pubkey given"));
        assert!(ssh::open_ssh(json!({ "pubkey": "" }),)
            .unwrap_err()
            .to_string()
            .starts_with("Empty ssh pubkey"));

        // its hard to test the ssh functionality as module test,
        // we would have to mock the iptables crate and make
        // AUTHORIZED_KEY_PATH configurable
    }

    //ToDo: add remaining unittests
}
