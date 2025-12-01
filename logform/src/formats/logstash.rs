use chrono::DateTime;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;

use crate::LogInfo;

use super::Format;

pub struct LogstashFormat;

impl Format for LogstashFormat {
    type Input = LogInfo;

    fn transform(&self, mut info: LogInfo) -> Option<Self::Input> {
        let mut logstash_object = json!({"@message": info.message});

        // The timestamp is expected to be a String in the meta map.
        let ts = match info.meta.remove("timestamp") {
            Some(Value::String(s)) => s,
            Some(Value::Number(num)) => {
                if let Some(epoch_secs) = num.as_i64() {
                    DateTime::<Utc>::from_timestamp(epoch_secs, 0)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_else(|| {
                            eprintln!("Invalid epoch_secs for timestamp: {}", epoch_secs);
                            Utc::now().to_rfc3339()
                        })
                } else {
                    eprintln!("Non-i64 number for timestamp: {}", num);
                    Utc::now().to_rfc3339()
                }
            }
            Some(other) => {
                eprintln!("Unexpected type for timestamp: {:?}", other);
                Utc::now().to_rfc3339()
            }
            None => Utc::now().to_rfc3339(),
        };
        logstash_object["@timestamp"] = json!(ts);

        let mut fields = HashMap::new();
        fields.insert("level".to_string(), json!(info.level.clone()));

        for (key, value) in info.meta.iter() {
            fields.insert(key.clone(), value.clone());
        }

        logstash_object["@fields"] = json!(fields);

        // Handle serialization errors gracefully
        match serde_json::to_string(&logstash_object) {
            Ok(serialized) => {
                info.message = serialized;
                Some(info)
            }
            Err(e) => {
                eprintln!("LogstashFormat: failed to serialize logstash object: {}", e);
                None
            }
        }
    }
}

pub fn logstash() -> LogstashFormat {
    LogstashFormat
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_logstash_format_with_timestamp() {
        let logstash_format = LogstashFormat;
        let mut info = LogInfo::new("info", "Test message");
        let timestamp_value = "2025-09-05T12:34:56Z";
        info.meta
            .insert("timestamp".to_string(), json!(timestamp_value));

        let result = logstash_format.transform(info).unwrap();
        let parsed: Value = serde_json::from_str(&result.message).unwrap();

        assert_eq!(parsed["@timestamp"], timestamp_value);
        assert_eq!(parsed["@message"], "Test message");
        assert_eq!(parsed["@fields"]["level"], "info");
    }
    use super::*;
    use serde_json::Value;

    #[test]
    fn test_logstash_format() {
        let logstash_format = LogstashFormat;

        let info = LogInfo::new("info", "Test message");
        let result = logstash_format.transform(info).unwrap();

        let parsed: Value = serde_json::from_str(&result.message).unwrap();
        assert!(parsed.get("@message").is_some());
        assert!(parsed.get("@fields").is_some());
        assert_eq!(parsed["@message"], "Test message");
        assert_eq!(parsed["@fields"]["level"], "info");
    }

    #[test]
    fn test_logstash_format_with_metadata() {
        let logstash_format = LogstashFormat;
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("user_id".to_string(), json!("1234"));
        info.meta
            .insert("transaction_id".to_string(), json!("abcd1234"));

        let result = logstash_format.transform(info).unwrap();
        let parsed: Value = serde_json::from_str(&result.message).unwrap();

        assert_eq!(parsed["@fields"]["user_id"], "1234");
        assert_eq!(parsed["@fields"]["transaction_id"], "abcd1234");
        assert_eq!(parsed["@fields"]["level"], "info");
    }

    #[test]
    fn test_metadata_preservation() {
        let logstash_format = LogstashFormat;
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("user_id".to_string(), json!("1234"));
        info.meta.insert("session_id".to_string(), json!("abcd"));

        let result = logstash_format.transform(info.clone()).unwrap();
        assert_eq!(result.meta.get("user_id").unwrap(), &json!("1234"));
        assert_eq!(result.meta.get("session_id").unwrap(), &json!("abcd"));

        let parsed: Value = serde_json::from_str(&result.message).unwrap();
        assert_eq!(parsed["@fields"]["user_id"], "1234");
        assert_eq!(parsed["@fields"]["session_id"], "abcd");
    }

    #[test]
    fn test_logstash_format_with_no_timestamp_in_meta() {
        let logstash_format = LogstashFormat;
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("user_id".to_string(), json!("1234"));

        let result = logstash_format.transform(info).unwrap();
        let parsed: Value = serde_json::from_str(&result.message).unwrap();

        // @timestamp should exist and be a valid ISO8601 string
        assert!(parsed.get("@timestamp").is_some());
        let ts = parsed["@timestamp"].as_str().unwrap();
        assert!(DateTime::parse_from_rfc3339(ts).is_ok());

        assert_eq!(parsed["@fields"]["user_id"], "1234");
    }
}
