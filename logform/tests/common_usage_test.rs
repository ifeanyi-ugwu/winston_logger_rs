use logform::{colorize, json, printf, simple, timestamp, FinalizeExt, Finalizer, Format, LogInfo};

#[test]
pub fn initialize_and_test_formats() {
    let log_info = LogInfo::new("info", "This is a test message");

    let colors = std::collections::HashMap::from([
        ("info".to_string(), serde_json::json!(["blue"])),
        ("error".to_string(), serde_json::json!(["red", "bold"])),
    ]);

    let format = timestamp()
        .chain(colorize().with_colors(colors).with_all(true))
        .finalize(printf(|info| {
            let timestamp = info
                .meta
                .get("timestamp")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            format!("{} - {}: {}", timestamp, info.level, info.message)
        }));

    let log_info = format
        .transform(log_info)
        .expect("Format chain transform failed");
    println!("{}", log_info.formatted.as_deref().unwrap_or(""));
}

#[test]
fn test_json() {
    let log_info = LogInfo::new("info", "This is a test message");

    let rendered = simple()
        .finalize(&log_info)
        .expect("Simple format finalize failed");
    println!("Simple format: {}", rendered);

    let log_info = LogInfo::new("info", "This is a test message");

    let rendered = json()
        .finalize(&log_info)
        .expect("JSON format finalize failed");
    println!("JSON format: {}", rendered);
}
