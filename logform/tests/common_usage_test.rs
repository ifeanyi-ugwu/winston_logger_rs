use logform::{chain, colorize, json, printf, simple, timestamp, Format, LogInfo};

#[test]
pub fn initialize_and_test_formats() {
    let log_info = LogInfo::new("info", "This is a test message");

    let colors = std::collections::HashMap::from([
        ("info".to_string(), serde_json::json!(["blue"])),
        ("error".to_string(), serde_json::json!(["red", "bold"])),
    ]);

    let format = chain!(
        timestamp(),
        colorize().with_colors(colors).with_all(true),
        printf(|info| {
            let timestamp = info
                .meta
                .get("timestamp")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            format!("{} - {}: {}", timestamp, info.level, info.message)
        }),
    );

    let log_info = format
        .transform(log_info)
        .expect("Format chain transform failed");
    println!("{}", log_info.message);
}

#[test]
fn test_json() {
    let log_info = LogInfo::new("info", "This is a test message");

    // Apply the simple format
    let simple_format = simple();
    let log_info = simple_format
        .transform(log_info)
        .expect("Simple format transform failed");
    println!("Simple format: {}", log_info.message);

    // Reset log_info for JSON format
    let log_info = LogInfo::new("info", "This is a test message");

    // Apply the JSON format
    let json_format = json();
    let log_info = json_format
        .transform(log_info)
        .expect("JSON format transform failed");
    println!("JSON format: {}", log_info.message);
}
