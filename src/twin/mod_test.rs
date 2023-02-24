#[cfg(test)]
mod mod_test {
    use super::super::*;
    use crate::test_util::Testrunner;
    use std::env;
    use std::sync::mpsc;
    use std::sync::mpsc::TryRecvError;
    use stdext::function_name;

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
        let tx = Arc::new(Mutex::new(tx));
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin.report_impl(json!({"test": "test"})).is_ok());

        assert_eq!(
            rx.recv().unwrap(),
            Message::Reported(json!({"test": "test"}))
        );
    }

    #[test]
    fn report_versions_test() {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(Mutex::new(tx));
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin.report_versions().is_ok());

        assert_eq!(
            rx.recv().unwrap(),
            Message::Reported(json!({
                "module-version": env!("CARGO_PKG_VERSION"),
                "azure-sdk-version": IotHubClient::get_sdk_version_string()
            }))
        );
    }

    #[test]
    fn update_and_report_general_consent_test() {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(Mutex::new(tx));
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

        let tr = Testrunner::new(function_name!().split("::").last().unwrap());
        tr.to_pathbuf("testfiles/consent_conf.json");

        env::set_var("CONSENT_DIR_PATH", tr.get_dirpath().as_str());

        assert!(twin.update_general_consent(None).is_ok());

        assert_eq!(
            rx.recv().unwrap(),
            Message::Reported(json!({"general_consent": ["swupdate"]}))
        );

        assert!(twin
            .update_general_consent(Some(json!(["SWUPDATE2", "SWUPDATE1"]).as_array().unwrap()))
            .is_ok());

        assert_eq!(
            rx.recv().unwrap(),
            Message::Reported(json!({"general_consent": ["swupdate1", "swupdate2"]}))
        );

        assert!(twin
            .update_general_consent(Some(json!(["swupdate1", "swupdate2"]).as_array().unwrap()))
            .is_ok());

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn report_user_consent_test() {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(Mutex::new(tx));
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
                tr.to_pathbuf("testfiles/invalid_consent.json")
                    .to_str()
                    .unwrap()
            )
            .unwrap_err()
            .to_string()
            .starts_with("report_user_consent: serde_json::from_reader"));

        assert!(twin
            .report_user_consent(
                tr.to_pathbuf("testfiles/history_consent.json")
                    .to_str()
                    .unwrap()
            )
            .is_ok());

        assert_eq!(
            rx.recv().unwrap(),
            Message::Reported(json!({
                "user_consent_history": {
                    "swupdate": [
                        "<version>"
                    ]
                }
            }))
        );
    }

    #[test]
    fn report_factory_reset_status_test() {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(Mutex::new(tx));
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin
            .report(&ReportProperty::FactoryResetStatus("in_progress"))
            .is_ok());

        let reported = format!("{:?}", rx.recv().unwrap());
        assert!(reported
            .contains("Reported(Object {\"factory_reset_status\": Object {\"date\": String"));
        assert!(reported.contains("\"status\": String(\"in_progress\")}})"));
    }

    #[test]
    fn report_factory_reset_result_test() {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(Mutex::new(tx));
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin.report_factory_reset_result().is_ok());

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        // ToDo: refactor report_factory_reset_result() for better testability
        // don't call "sh -c fw_printenv factory-reset-status" command in function
        // but inject command output
    }

    #[test]
    fn update_and_report_network_status_test() {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(Mutex::new(tx));
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        assert!(twin.update_include_network_filter(None).is_ok());

        assert_eq!(
            rx.recv().unwrap(),
            Message::Reported(json!({ "network_interfaces": json!(null) }))
        );

        assert!(twin
            .update_include_network_filter(Some(json!(["*"]).as_array().unwrap()))
            .is_ok());

        assert!(rx.recv().is_ok());

        assert!(twin
            .update_include_network_filter(Some(json!(["*"]).as_array().unwrap()))
            .is_ok());

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

        // ToDo: add more tests based on network adapters available on build server?
    }
}
