use super::Format;
use crate::LogInfo;
use chrono::Utc;
use serde_json::json;

#[derive(Clone, Default)]
pub struct Timestamp {
    format: Option<String>,
    alias: Option<String>,
}

impl Timestamp {
    pub fn new() -> Self {
        Self {
            format: None,
            alias: None,
        }
    }

    pub fn with_format(mut self, format: &str) -> Self {
        self.format = Some(format.to_string());
        self
    }

    pub fn with_alias(mut self, alias: &str) -> Self {
        self.alias = Some(alias.to_string());
        self
    }

    pub fn transform(&self, mut info: LogInfo) -> Option<LogInfo> {
        let timestamp = if let Some(fmt) = &self.format {
            Utc::now().format(fmt).to_string()
        } else {
            Utc::now().to_rfc3339()
        };

        // Always set the timestamp field
        info.meta.insert("timestamp".to_string(), json!(&timestamp));

        // Set alias if provided
        if let Some(alias) = &self.alias {
            info.meta.insert(alias.clone(), json!(&timestamp));
        }

        Some(info)
    }
}

impl Format for Timestamp {
    type Input = LogInfo;

    fn transform(&self, info: LogInfo) -> Option<Self::Input> {
        self.transform(info)
    }
}

pub fn timestamp() -> Timestamp {
    Timestamp::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn test_default_timestamp() {
        let formatter = timestamp();
        let info = LogInfo::new("info", "Test message");
        let result = formatter.transform(info).unwrap();

        assert!(result.meta.contains_key("timestamp"));
        let timestamp = result.meta.get("timestamp").unwrap().as_str().unwrap();

        // RFC3339 allows variable precision for fractional seconds (0-9 digits)
        // The regex should accept any valid RFC3339 format
        let rfc3339_regex =
            Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?([+-]\d{2}:\d{2}|Z)$")
                .unwrap();
        assert!(
            rfc3339_regex.is_match(timestamp),
            "Timestamp '{}' does not match RFC3339 format",
            timestamp
        );
    }

    #[test]
    fn test_custom_format() {
        let formatter = timestamp().with_format("%d/%m/%Y %H:%M:%S");
        let info = LogInfo::new("info", "Test message");
        let result = formatter.transform(info).unwrap();

        assert!(result.meta.contains_key("timestamp"));
        let timestamp = result.meta.get("timestamp").unwrap().as_str().unwrap();
        let custom_format_regex = Regex::new(r"^\d{2}/\d{2}/\d{4} \d{2}:\d{2}:\d{2}$").unwrap();
        assert!(custom_format_regex.is_match(timestamp));
    }

    #[test]
    fn test_alias() {
        let formatter = timestamp().with_alias("log_time");
        let info = LogInfo::new("info", "Test message");
        let result = formatter.transform(info).unwrap();

        assert!(result.meta.contains_key("timestamp"));
        assert!(result.meta.contains_key("log_time"));
        assert_eq!(result.meta.get("timestamp"), result.meta.get("log_time"));
    }

    #[test]
    fn test_custom_format_with_alias() {
        let formatter = timestamp()
            .with_format("%d/%m/%Y %H:%M:%S")
            .with_alias("log_time");
        let info = LogInfo::new("info", "Test message");
        let result = formatter.transform(info).unwrap();

        assert!(result.meta.contains_key("timestamp"));
        assert!(result.meta.contains_key("log_time"));
        let timestamp = result.meta.get("timestamp").unwrap().as_str().unwrap();
        let log_time = result.meta.get("log_time").unwrap().as_str().unwrap();

        assert_eq!(timestamp, log_time);
        let custom_format_regex = Regex::new(r"^\d{2}/\d{2}/\d{4} \d{2}:\d{2}:\d{2}$").unwrap();
        assert!(custom_format_regex.is_match(timestamp));
    }
}
