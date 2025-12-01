mod common;

use common::MockTransport;
use logform::LogInfo;
use winston::Logger;

#[test]
fn test_global_close() {
    if !winston::is_initialized() {
        winston::init(Logger::new(None));
    }

    let transport = MockTransport::new();
    winston::add_transport(transport.clone());

    winston::log(LogInfo::new("info", "Before close"));
    winston::close();

    assert_eq!(transport.log_count(), 1);
}
