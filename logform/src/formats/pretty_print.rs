use crate::{utils::format_json::format_json, LogInfo};
use serde_json::{Map, Value};

use super::Format;

#[derive(Clone)]
pub struct PrettyPrinter {
    colorize: bool,
}

impl Default for PrettyPrinter {
    fn default() -> Self {
        Self::new()
    }
}

impl PrettyPrinter {
    pub fn new() -> Self {
        PrettyPrinter { colorize: false }
    }

    pub fn with_colorize(mut self, colorize: bool) -> Self {
        self.colorize = colorize;
        self
    }

    fn format_log(&self, info: LogInfo) -> LogInfo {
        let mut json_output = Map::new();
        json_output.insert("level".to_string(), Value::String(info.level.clone()));
        json_output.insert("message".to_string(), Value::String(info.message.clone()));

        for (key, value) in info.meta {
            json_output.insert(key, value);
        }

        let json_value = Value::Object(json_output);
        let pretty_message = format_json(&json_value, self.colorize);

        LogInfo {
            level: info.level.clone(),
            message: pretty_message,
            meta: std::collections::HashMap::new(),
        }
    }
}

impl Format for PrettyPrinter {
    type Input = LogInfo;

    fn transform(&self, info: LogInfo) -> Option<Self::Input> {
        Some(self.format_log(info))
    }
}

pub fn pretty_print() -> PrettyPrinter {
    PrettyPrinter::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use serde_json::{json, Value};

    #[test]
    fn test_pretty_print_json_structure() {
        let formatter = pretty_print();

        let info = LogInfo::new("info", "User logged in")
            .with_meta("user_id", 12345)
            .with_meta("session_id", "abcde12345")
            .with_meta(
                "extra_info",
                json!({
                    "null": null,
                    "number": 1,
                    "boolean": true,
                    "inner_object": {
                        "null": null,
                        "number": 1,
                        "boolean": true
                    }
                }),
            )
            .with_meta("empty object", json!({}))
            .with_meta("empty array", json!([]));

        let result = formatter.transform(info).unwrap();

        // Check for overall structure
        let message = &result.message;

        // Check for proper JSON-like structure
        assert!(message.starts_with("{"), "Message should start with '{{'");
        assert!(message.ends_with("}"), "Message should end with '}}'");

        // Ensure key-value pairs exist correctly
        assert!(
            message.contains("level: 'info'"),
            "Missing or incorrect level field"
        );
        assert!(
            message.contains("message: 'User logged in'"),
            "Missing or incorrect message field"
        );
        assert!(
            message.contains("session_id: 'abcde12345'"),
            "Missing or incorrect session_id"
        );
        assert!(
            message.contains("user_id: 12345"),
            "Missing or incorrect user_id"
        );

        // Check special values
        assert!(
            message.contains("null: null"),
            "Null values should be formatted correctly"
        );
        assert!(
            message.contains("boolean: true"),
            "Boolean values should be formatted correctly"
        );

        // Ensure empty structures are formatted properly
        assert!(
            message.contains("empty object: {}"),
            "Empty objects should be formatted as '{{}}'"
        );
        assert!(
            message.contains("empty array: []"),
            "Empty arrays should be formatted as '[]'"
        );

        // Check for nested objects
        assert!(
            message.contains("extra_info: {"),
            "Should contain 'extra_info' object"
        );
        assert!(
            message.contains("inner_object: {"),
            "Should contain 'inner_object' object"
        );

        // Check for presence of keys rather than order (HashMap doesn't guarantee ordering)
        println!("Actual message: {}", message);
        assert!(
            message.contains("inner_object: {"),
            "Should have inner_object"
        );
        assert!(message.contains("boolean: true"), "Should have boolean");
        assert!(message.contains("null: null"), "Should have null");
        assert!(message.contains("number: 1"), "Should have number");
        assert!(message.contains("extra_info: {"), "Should have extra_info");
    }

    #[test]
    fn test_pretty_print_colorization() {
        let formatter = pretty_print().with_colorize(true);

        let info = LogInfo::new("info", "Test message")
            .with_meta("string_value", "test string")
            .with_meta("number_value", 12345)
            .with_meta("bool_value", true)
            .with_meta("null_value", Value::Null);

        let result = formatter.transform(info).unwrap();
        let message = &result.message;

        let re_info = Regex::new(r"level: '\x1b\[32minfo\x1b\[0m'").unwrap();
        let re_message = Regex::new(r"message: '\x1b\[32mTest message\x1b\[0m'").unwrap();
        let re_string = Regex::new(r"string_value: '\x1b\[32mtest string\x1b\[0m'").unwrap();
        let re_number = Regex::new(r"number_value: \x1b\[34m12345\x1b\[0m").unwrap();
        let re_bool = Regex::new(r"bool_value: \x1b\[33mtrue\x1b\[0m").unwrap();
        let re_null = Regex::new(r"null_value: \x1b\[31mnull\x1b\[0m").unwrap();

        // Check if colored output matches
        assert!(re_info.is_match(message), "Missing green color for 'info'");
        assert!(
            re_message.is_match(message),
            "Missing green color for 'Test message'"
        );
        assert!(
            re_string.is_match(message),
            "Missing green color for 'test string'"
        );
        assert!(
            re_number.is_match(message),
            "Missing blue color for number 12345"
        );
        assert!(
            re_bool.is_match(message),
            "Missing yellow color for boolean true"
        );
        assert!(re_null.is_match(message), "Missing red color for null");
    }
}
