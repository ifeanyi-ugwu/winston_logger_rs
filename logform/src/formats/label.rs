use super::Format;
use crate::LogInfo;
use serde_json::json;

pub struct LabelFormat {
    label: String,
    message: bool,
}

impl Default for LabelFormat {
    fn default() -> Self {
        Self::new()
    }
}

impl LabelFormat {
    pub fn new() -> Self {
        Self {
            label: String::new(),
            message: false,
        }
    }

    pub fn with_label(mut self, label: &str) -> Self {
        self.label = label.to_string();
        self
    }

    pub fn with_message(mut self, apply: bool) -> Self {
        self.message = apply;
        self
    }
}

impl Format for LabelFormat {
    type Input = LogInfo;

    fn transform(&self, mut info: LogInfo) -> Option<Self::Input> {
        if self.message {
            info.message = format!("[{}] {}", self.label, info.message);
        } else {
            info.meta.insert("label".to_string(), json!(self.label));
        }
        Some(info)
    }
}

pub fn label() -> LabelFormat {
    LabelFormat::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_label_format_empty_label_message() {
        let label_format = LabelFormat::new().with_label("").with_message(true);
        let info = LogInfo::new("info", "Test message");
        let result = label_format.transform(info).unwrap();
        assert_eq!(result.message, "[] Test message");
    }

    #[test]
    fn test_label_format_overwrite_existing_label_meta() {
        let label_format = LabelFormat::new()
            .with_label("NEW_LABEL")
            .with_message(false);
        let mut info = LogInfo::new("info", "Test message");
        info.meta.insert("label".to_string(), json!("OLD_LABEL"));
        let result = label_format.transform(info).unwrap();
        assert_eq!(result.meta.get("label"), Some(&json!("NEW_LABEL")));
    }

    #[test]
    fn test_label_format_empty_message() {
        let label_format = LabelFormat::new().with_label("LABEL").with_message(true);
        let info = LogInfo::new("info", "");
        let result = label_format.transform(info).unwrap();
        assert_eq!(result.message, "[LABEL] ");
    }
    use super::*;

    #[test]
    fn test_label_format_message() {
        let label_format = LabelFormat::new().with_label("MY_LABEL").with_message(true);
        let info = LogInfo::new("info", "Test message");
        let result = label_format.transform(info).unwrap();
        assert_eq!(result.message, "[MY_LABEL] Test message");
    }

    #[test]
    fn test_label_format_meta() {
        let label_format = LabelFormat::new()
            .with_label("MY_LABEL")
            .with_message(false);
        let info = LogInfo::new("info", "Test message");
        let result = label_format.transform(info).unwrap();
        assert_eq!(result.meta.get("label"), Some(&json!("MY_LABEL")));
    }
}
