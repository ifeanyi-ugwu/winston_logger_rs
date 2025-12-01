use logform::LogInfo;

#[test]
#[should_panic(expected = "Global logger not initialized")]
fn test_log_without_init_panics() {
    winston::log(LogInfo::new("info", "Should panic"));
}

#[test]
fn test_try_log_when_not_initialized() {
    let result = winston::try_log(LogInfo::new("info", "Should fail"));
    assert!(!result);
}

#[test]
fn test_global_try_log_when_not_initialized() {
    if !winston::is_initialized() {
        let result = winston::try_log(LogInfo::new("info", "Should fail"));
        assert!(!result);
    }
}
