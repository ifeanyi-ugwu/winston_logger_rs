#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use serde_json::Value;
use std::{collections::HashMap, fmt, str::FromStr};

#[cfg(feature = "serde")]
use std::io::Result as IoResult;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LogInfo {
    pub level: String,
    pub message: String,
    pub meta: HashMap<String, Value>,
}

impl LogInfo {
    pub fn new<L: Into<String>, M: Into<String>>(level: L, message: M) -> Self {
        Self {
            level: level.into(),
            message: message.into(),
            meta: HashMap::new(),
        }
    }

    pub fn with_meta<K, V>(mut self, key: K, value: V) -> Self
    where
        K: Into<String>,
        V: Into<Value>,
    {
        self.meta.insert(key.into(), value.into());
        self
    }

    pub fn without_meta<K: Into<String>>(mut self, key: K) -> Self {
        self.meta.remove(&key.into());
        self
    }

    /// Convert LogInfo to JSON bytes
    #[cfg(feature = "serde")]
    pub fn to_bytes(&self) -> IoResult<Vec<u8>> {
        serde_json::to_vec(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Convert JSON bytes to LogInfo
    #[cfg(feature = "serde")]
    pub fn from_bytes(bytes: &[u8]) -> IoResult<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Convert serde_json::Value to LogInfo
    pub fn from_value(value: Value) -> Result<Self, String> {
        if let Value::Object(map) = value {
            let level = map
                .get("level")
                .and_then(Value::as_str)
                .ok_or("Missing or invalid 'level' field")?
                .to_string();

            let message = map
                .get("message")
                .and_then(Value::as_str)
                .ok_or("Missing or invalid 'message' field")?
                .to_string();

            let mut meta = HashMap::new();
            if let Some(meta_value) = map.get("meta") {
                if let Value::Object(meta_map) = meta_value.clone() {
                    for (key, value) in meta_map {
                        meta.insert(key, value);
                    }
                }
            }

            Ok(Self {
                level,
                message,
                meta,
            })
        } else {
            Err("Input value is not a JSON object".to_string())
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "level": self.level,
            "message": self.message,
            "meta": self.meta,
        })
    }

    /// Returns a flattened JSON representation where metadata fields are at the root level.
    /// This is used by transports for consistent serialization and querying.
    /// Users query fields directly without "meta." prefix.
    pub fn to_flat_value(&self) -> Value {
        let mut flat = serde_json::Map::new();
        flat.insert("level".to_string(), Value::String(self.level.clone()));
        flat.insert("message".to_string(), Value::String(self.message.clone()));

        // Merge all metadata fields at root level
        for (key, value) in &self.meta {
            flat.insert(key.clone(), value.clone());
        }

        Value::Object(flat)
    }
}

#[macro_export]
macro_rules! log_info {
    // Without metadata
    ($level:ident, $msg:expr) => {{
        $crate::LogInfo::new(stringify!($level), $msg)
    }};

    // With metadata
    ($level:ident, $msg:expr, $($key:ident = $value:expr),*) => {{
        let mut log_entry = $crate::LogInfo::new(stringify!($level), $msg);
        $(
            log_entry = log_entry.with_meta(stringify!($key), $value);
        )*
        log_entry
    }};
}

impl fmt::Display for LogInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only write the message field, which has already been formatted by formatters.
        // Formatters are responsible for including level, timestamp, and other fields
        // in the message as needed.
        write!(f, "{}", self.message)
    }
}

impl FromStr for LogInfo {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Try to parse as JSON first
        if let Ok(value) = serde_json::from_str::<Value>(s) {
            return LogInfo::from_value(value);
        }

        // Fallback: Parse simple format "[LEVEL] message"
        let s = s.trim();

        // Check for bracketed level
        if !s.starts_with('[') {
            return Err("Expected log to start with '[LEVEL]'".to_string());
        }

        let end_bracket = s.find(']').ok_or("Missing closing bracket for level")?;

        let level = s[1..end_bracket].to_string();
        let rest = s[end_bracket + 1..].trim();

        // Split message and metadata if present
        if let Some(meta_start) = rest.find('{') {
            let message = rest[..meta_start].trim().to_string();
            let meta_str = &rest[meta_start..];

            // Parse metadata (simple key: value parsing)
            let mut meta = HashMap::new();
            if let Some(meta_end) = meta_str.rfind('}') {
                let meta_content = &meta_str[1..meta_end];
                for pair in meta_content.split(',') {
                    let parts: Vec<&str> = pair.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let key = parts[0].trim().to_string();
                        let value_str = parts[1].trim();

                        // Try to parse as JSON value
                        let value = serde_json::from_str(value_str)
                            .unwrap_or_else(|_| Value::String(value_str.to_string()));

                        meta.insert(key, value);
                    }
                }
            }

            Ok(LogInfo {
                level,
                message,
                meta,
            })
        } else {
            // No metadata
            Ok(LogInfo {
                level,
                message: rest.to_string(),
                meta: HashMap::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(feature = "serde")]
    #[test]
    fn test_byte_serialization_and_deserialization() {
        let log = LogInfo::new("INFO", "Test message")
            .with_meta("user", "Alice")
            .with_meta("attempts", 3);

        let json_bytes = log.to_bytes().expect("Failed to serialize to JSON");
        let deserialized_log =
            LogInfo::from_bytes(&json_bytes).expect("Failed to deserialize JSON");

        assert_eq!(deserialized_log.level, "INFO");
        assert_eq!(deserialized_log.message, "Test message");
        assert_eq!(deserialized_log.meta["user"], json!("Alice"));
        assert_eq!(deserialized_log.meta["attempts"], json!(3));
    }

    #[test]
    fn test_from_value() {
        let json_value = json!({
            "level": "DEBUG",
            "message": "Another test message",
            "meta": {
                "id": 12345,
                "status": "pending"
            }
        });

        let log_info =
            LogInfo::from_value(json_value).expect("Failed to create LogInfo from Value");

        assert_eq!(log_info.level, "DEBUG");
        assert_eq!(log_info.message, "Another test message");
        assert_eq!(log_info.meta["id"], json!(12345));
        assert_eq!(log_info.meta["status"], json!("pending"));
    }
}

#[cfg(test)]
mod display_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_display_without_meta() {
        let log = LogInfo::new("INFO", "Test message");
        // Display only outputs the message field (formatters handle full formatting)
        assert_eq!(format!("{}", log), "Test message");
    }

    #[test]
    fn test_display_with_meta() {
        let log = LogInfo::new("ERROR", "Connection failed")
            .with_meta("retry", 3)
            .with_meta("host", "example.com");

        // Display only outputs the message field (formatters handle full formatting)
        let display = format!("{}", log);
        assert_eq!(display, "Connection failed");
    }

    #[test]
    fn test_from_str_simple() {
        let log: LogInfo = "[WARN] Something happened".parse().unwrap();
        assert_eq!(log.level, "WARN");
        assert_eq!(log.message, "Something happened");
        assert!(log.meta.is_empty());
    }

    #[test]
    fn test_from_str_with_meta() {
        let input = r#"[DEBUG] Processing {user: "Alice", count: 5}"#;
        let log: LogInfo = input.parse().unwrap();
        assert_eq!(log.level, "DEBUG");
        assert_eq!(log.message, "Processing");
        assert_eq!(log.meta.get("count").unwrap(), &json!(5));
    }

    #[test]
    fn test_from_str_json() {
        let json_str = r#"{"level":"INFO","message":"Test","meta":{"id":123}}"#;
        let log: LogInfo = json_str.parse().unwrap();
        assert_eq!(log.level, "INFO");
        assert_eq!(log.message, "Test");
        assert_eq!(log.meta.get("id").unwrap(), &json!(123));
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_roundtrip() {
        let original = LogInfo::new("INFO", "Test message").with_meta("key", "value");

        let json_str = serde_json::to_string(&original).unwrap();
        let parsed: LogInfo = json_str.parse().unwrap();

        assert_eq!(parsed.level, original.level);
        assert_eq!(parsed.message, original.message);
        assert_eq!(parsed.meta, original.meta);
    }
}
