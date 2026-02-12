use crate::LogInfo;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;

use super::Format;

pub struct MetadataFormat {
    key: String,
    fill_except: HashSet<String>,
    fill_with: HashSet<String>,
}

impl Default for MetadataFormat {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataFormat {
    pub fn new() -> Self {
        MetadataFormat {
            key: "metadata".to_string(),
            fill_except: HashSet::new(),
            fill_with: HashSet::new(),
        }
    }

    pub fn with_key(mut self, key: &str) -> Self {
        self.key = key.to_string();
        self
    }

    pub fn with_fill_except(mut self, keys: Vec<&str>) -> Self {
        self.fill_except = keys.into_iter().map(String::from).collect();
        self
    }

    pub fn with_fill_with(mut self, keys: Vec<&str>) -> Self {
        self.fill_with = keys.into_iter().map(String::from).collect();
        self
    }
}

impl Format for MetadataFormat {
    type Input = LogInfo;

    fn transform(&self, mut info: LogInfo) -> Option<Self::Input> {
        let mut metadata = HashMap::new();

        if !self.fill_with.is_empty() {
            for key in &self.fill_with {
                if self.fill_except.contains(key) {
                    continue;
                }
                if let Some(value) = info.meta.remove(key) {
                    metadata.insert(key.clone(), value);
                }
            }
        } else {
            // Collect keys to move to avoid cloning the whole map
            let keys_to_move: Vec<String> = info
                .meta
                .keys()
                .filter(|key| !self.fill_except.contains(*key))
                .cloned()
                .collect();
            for key in keys_to_move {
                if let Some(value) = info.meta.remove(&key) {
                    metadata.insert(key, value);
                }
            }
        }

        info.meta.insert(self.key.clone(), json!(metadata));
        Some(info)
    }
}

pub fn metadata() -> MetadataFormat {
    MetadataFormat::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_metadata_with_fill_with_and_fill_except() {
        let metadata_format = MetadataFormat::new()
            .with_key("metadata")
            .with_fill_with(vec!["key1", "key2", "key3"])
            .with_fill_except(vec!["key2"]);
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("key1".to_string(), "value1".into());
        info.meta.insert("key2".to_string(), "value2".into());
        info.meta.insert("key3".to_string(), "value3".into());

        let result = metadata_format.transform(info).unwrap();
        let metadata = result.meta.get("metadata").unwrap();

        // Only key1 and key3 should be present, key2 excluded
        assert_eq!(
            metadata.get("key1"),
            Some(&Value::String("value1".to_string()))
        );
        assert_eq!(
            metadata.get("key3"),
            Some(&Value::String("value3".to_string()))
        );
        assert!(metadata.get("key2").is_none());
    }

    #[test]
    fn test_metadata_with_empty_meta() {
        let metadata_format = MetadataFormat::new().with_key("metadata");
        let info = LogInfo::new("info", "Test message");
        let result = metadata_format.transform(info).unwrap();
        let metadata = result.meta.get("metadata").unwrap();
        assert!(metadata.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_metadata_with_fill_with_nonexistent_keys() {
        let metadata_format = MetadataFormat::new()
            .with_key("metadata")
            .with_fill_with(vec!["not_present", "also_missing"]);
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("key1".to_string(), "value1".into());
        let result = metadata_format.transform(info).unwrap();
        let metadata = result.meta.get("metadata").unwrap();
        // Should be empty since none of the fill_with keys exist
        assert!(metadata.as_object().unwrap().is_empty());
        // Original meta should remain unchanged
        assert_eq!(
            result.meta.get("key1"),
            Some(&Value::String("value1".to_string()))
        );
    }

    #[test]
    fn test_metadata_format_default_constructor() {
        let metadata_format = MetadataFormat::new();
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("key1".to_string(), "value1".into());
        let result = metadata_format.transform(info).unwrap();
        // By default, all keys should be moved into the default key
        let key = &metadata_format.key;
        let metadata = result.meta.get(key).unwrap();
        assert_eq!(
            metadata.get("key1"),
            Some(&Value::String("value1".to_string()))
        );
        // Should be removed from original meta
        assert!(!result.meta.contains_key("key1"));
    }
    use super::*;
    use serde_json::Value;

    #[test]
    fn test_metadata_with_fill_with() {
        let metadata_format = MetadataFormat::new()
            .with_key("metadata")
            .with_fill_with(vec!["key1"]);
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("key1".to_string(), "value1".into());
        info.meta.insert("key2".to_string(), "value2".into());

        let result = metadata_format.transform(info).unwrap();
        let metadata = result.meta.get("metadata").unwrap();

        assert_eq!(
            metadata.get("key1"),
            Some(&Value::String("value1".to_string()))
        );
        assert!(metadata.get("key2").is_none());
        assert!(!result.meta.contains_key("key1"));
        assert_eq!(
            result.meta.get("key2"),
            Some(&Value::String("value2".to_string()))
        );
    }

    #[test]
    fn test_metadata_with_fill_except() {
        let metadata_format = MetadataFormat::new()
            .with_key("metadata")
            .with_fill_except(vec!["key1", "key3"]);
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("key1".to_string(), "value1".into());
        info.meta.insert("key2".to_string(), "value2".into());
        info.meta.insert("key3".to_string(), "value3".into());

        let result = metadata_format.transform(info).unwrap();
        let metadata = result.meta.get("metadata").unwrap();

        assert_eq!(
            metadata.get("key2"),
            Some(&Value::String("value2".to_string()))
        );
        assert!(metadata.get("key1").is_none());
        assert!(metadata.get("key3").is_none());
        assert_eq!(
            result.meta.get("key1"),
            Some(&Value::String("value1".to_string()))
        );
        assert_eq!(
            result.meta.get("key3"),
            Some(&Value::String("value3".to_string()))
        );
    }
}
