#[cfg(test)]
mod mod_test {
    use super::super::*;
    use crate::test_util::Testrunner;

    #[test]
    fn factory_reset_test() {
        assert!(reset_to_factory_settings(json!({
            "type": 1,
            "restore_settings": ["wifi"]
        }),)
        .unwrap_err()
        .to_string()
        .starts_with("No such file or directory"));

        assert!(reset_to_factory_settings(json!({
            "type": 1,
        }),)
        .unwrap_err()
        .to_string()
        .starts_with("No such file or directory"));

        assert_eq!(
            reset_to_factory_settings(json!({
                "restore_settings": ["wifi"]
            }),)
            .unwrap_err()
            .to_string(),
            "reset type missing or not supported"
        );

        assert_eq!(
            reset_to_factory_settings(json!({
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
        assert!(user_consent(json!({"swupdate": "1.0.0"}))
            .unwrap_err()
            .to_string()
            .starts_with("user_consent: open user_consent.json for write"));

        assert_eq!(
            user_consent(json!({})).unwrap_err().to_string(),
            "unexpected parameter format"
        );

        assert_eq!(
            user_consent(json!({"swupdate": "1.0.0", "another_component": "1.2.3"}))
                .unwrap_err()
                .to_string(),
            "unexpected parameter format"
        );
    }
}
