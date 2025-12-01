mod common;

use common::MockTransport;
use logform::LogInfo;
use winston::Logger;

#[test]
fn test_global_configure() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

    let transport = MockTransport::new();

    winston::configure(Some(
        winston::LoggerOptions::new()
            .level("error")
            .transport(transport.clone()),
    ));

    winston::log(LogInfo::new("info", "Filtered"));
    winston::log(LogInfo::new("error", "Passes"));
    winston::flush().unwrap();

    assert_eq!(transport.log_count(), 1);
}
