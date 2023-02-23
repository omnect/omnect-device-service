#[cfg(test)]
mod mod_test {
    use super::super::*;
    use crate::test_util::Testrunner;
    use std::env;
    use std::sync::mpsc;
    use std::sync::mpsc::TryRecvError;
    use stdext::function_name;

    #[test]
    fn update_and_report_general_consent_test() {
        let tr = Testrunner::new(function_name!().split("::").last().unwrap());
        let _ = tr.to_pathbuf("testfiles/consent_conf.json");
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(Mutex::new(tx));
        let mut twin = Twin {
            tx: Some(tx),
            include_network_filter: None,
        };

        env::set_var("CONSENT_DIR_PATH", tr.get_dirpath().as_str());

        assert!(twin.update_general_consent(None).is_ok());

        assert_eq!(
            rx.recv().unwrap(),
            Message::Reported(json!({"general_consent": ["swupdate"]}))
        );

        assert!(twin
            .update_general_consent(Some(json!(["swupdate1", "swupdate2"]).as_array().unwrap()))
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
    fn report_factory_reset_test() {
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
}
