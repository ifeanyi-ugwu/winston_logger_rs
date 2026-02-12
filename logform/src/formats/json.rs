use super::Format;
use crate::LogInfo;
use serde_json::{Map, Value};

pub struct JsonFormat;

impl Format for JsonFormat {
    type Input = LogInfo;

    fn transform(&self, info: LogInfo) -> Option<Self::Input> {
        let mut log_object = Map::new();

        log_object.insert("level".to_string(), Value::String(info.level.clone()));
        log_object.insert("message".to_string(), Value::String(info.message.clone()));

        for (key, value) in info.meta.into_iter() {
            log_object.insert(key, value);
        }

        let json_message = Value::Object(log_object).to_string();

        // Clear meta to avoid duplication and extra memory use
        Some(LogInfo {
            level: info.level,
            message: json_message,
            meta: std::collections::HashMap::new(),
        })
    }
}

pub fn json() -> JsonFormat {
    JsonFormat
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_json_format_empty_metadata() {
        let json_formatter = JsonFormat;
        let info = LogInfo::new("info", "User logged in");
        let result = json_formatter.transform(info).unwrap();
        let expected_value = json!({
            "level": "info",
            "message": "User logged in"
        });
        let actual_value: Value = serde_json::from_str(&result.message).unwrap();
        assert_eq!(actual_value, expected_value);
    }

    #[test]
    fn test_json_format_special_characters() {
        let json_formatter = JsonFormat;
        let info = LogInfo::new("info", "Special chars: \" \n \t ")
            .with_meta("weird\nkey", Value::String("strange\tvalue".to_string()));
        let result = json_formatter.transform(info).unwrap();
        let expected_value = json!({
            "level": "info",
            "message": "Special chars: \" \n \t ",
            "weird\nkey": "strange\tvalue"
        });
        let actual_value: Value = serde_json::from_str(&result.message).unwrap();
        assert_eq!(actual_value, expected_value);
    }

    #[test]
    fn test_json_format_large_metadata() {
        let json_formatter = JsonFormat;
        let mut info = LogInfo::new("info", "Bulk meta test");
        for i in 0..1000 {
            info.meta
                .insert(format!("key_{}", i), Value::Number(i.into()));
        }
        let result = json_formatter.transform(info).unwrap();
        let mut expected = serde_json::Map::new();
        expected.insert("level".to_string(), Value::String("info".to_string()));
        expected.insert(
            "message".to_string(),
            Value::String("Bulk meta test".to_string()),
        );
        for i in 0..1000 {
            expected.insert(format!("key_{}", i), Value::Number(i.into()));
        }
        let expected_value = Value::Object(expected);

        // Compare as parsed values to avoid HashMap key ordering issues
        let actual_value: Value = serde_json::from_str(&result.message).unwrap();
        assert_eq!(actual_value, expected_value);
    }

    #[test]
    fn test_json_format_empty_level_and_message() {
        let json_formatter = JsonFormat;
        let info = LogInfo::new("", "");
        let result = json_formatter.transform(info).unwrap();
        let expected_value = json!({
            "level": "",
            "message": ""
        });
        let actual_value: Value = serde_json::from_str(&result.message).unwrap();
        assert_eq!(actual_value, expected_value);
    }
    use serde_json::json;

    use super::*;

    #[test]
    fn test_json_format() {
        let json_formatter = JsonFormat;

        let info = LogInfo::new("info", "User logged in")
            .with_meta("user_id", Value::Number(12345.into()))
            .with_meta("session_id", Value::String("abcde12345".to_string()));

        let result = json_formatter.transform(info).unwrap();
        let expected_value = json!({
            "level": "info",
            "message": "User logged in",
            "user_id": 12345,
            "session_id": "abcde12345"
        });

        let actual_value: Value = serde_json::from_str(&result.message).unwrap();
        assert_eq!(actual_value, expected_value);
    }
}
