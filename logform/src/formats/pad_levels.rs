use crate::LogInfo;
use std::collections::{HashMap, HashSet};

use super::Format;

#[derive(Clone)]
pub struct Padder {
    levels: HashSet<String>,
    filler: String,
    paddings: HashMap<String, String>,
}

impl Padder {
    pub fn new() -> Self {
        let levels: HashSet<String> = crate::config::rust::levels().into_keys().collect();
        let filler = " ".to_string();
        let paddings = Self::padding_for_levels(&levels, &filler);

        Padder {
            levels,
            filler,
            paddings,
        }
    }

    pub fn with_levels(mut self, levels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.levels = levels.into_iter().map(Into::into).collect();
        self.paddings = Self::padding_for_levels(&self.levels, &self.filler);
        self
    }

    pub fn with_filler(mut self, filler: String) -> Self {
        self.filler = filler;
        self.paddings = Self::padding_for_levels(&self.levels, &self.filler);
        self
    }

    fn get_longest_level(levels: &HashSet<String>) -> usize {
        levels.iter().map(|level| level.len()).max().unwrap_or(0)
    }

    fn padding_for_levels(levels: &HashSet<String>, filler: &str) -> HashMap<String, String> {
        let max_length = Self::get_longest_level(levels);
        levels
            .iter()
            .map(|level| {
                let padding = Self::padding_for_level(level, filler, max_length);
                (level.clone(), padding)
            })
            .collect()
    }

    fn padding_for_level(level: &str, filler: &str, max_length: usize) -> String {
        let target_len = max_length + 1 - level.len();
        filler.repeat(target_len).chars().take(target_len).collect()
    }

    pub fn transform(&self, mut info: LogInfo) -> Option<LogInfo> {
        if let Some(padding) = self.paddings.get(&info.level) {
            info.message = format!("{}{}", padding, info.message);
        }
        Some(info)
    }
}

impl Format for Padder {
    type Input = LogInfo;

    fn transform(&self, info: LogInfo) -> Option<Self::Input> {
        self.transform(info)
    }
}

pub fn pad_levels() -> Padder {
    Padder::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LogInfo;
    use std::collections::HashMap;

    #[test]
    fn test_padder_with_padding() {
        let levels = vec!["info".to_string(), "error".to_string()];
        let padder = Padder::new().with_levels(levels.iter());

        let log_info = LogInfo::new("error", "Test message");
        let transformed = padder.transform(log_info).unwrap();

        assert_eq!(transformed.message, " Test message");
    }

    #[test]
    fn test_padder_with_custom_filler() {
        let levels = vec![
            "info".to_string(),
            "debug".to_string(),
            "critical".to_string(),
        ];
        let padder = Padder::new()
            .with_levels(levels.iter())
            .with_filler("#".to_string());

        let log_info = LogInfo::new("debug", "Test message");
        let transformed = padder.transform(log_info).unwrap();

        let log_info = LogInfo::new("info", "Test message");
        let transformed_2 = padder.transform(log_info).unwrap();

        assert_eq!(transformed.message, "####Test message");
        assert_eq!(transformed_2.message, "#####Test message");
    }

    #[test]
    fn test_padlevels_function() {
        let levels = HashMap::from([
            ("info".to_string(), "info".to_string()),
            ("error".to_string(), "error".to_string()),
            ("critical".to_string(), "critical".to_string()),
        ]);
        let padder = Padder::new()
            .with_levels(levels.keys())
            .with_filler("-".to_string());

        let info = LogInfo::new("info", "Custom filler message");
        let result_info = padder.transform(info).unwrap();

        let error = LogInfo::new("error", "Error message");
        let result_error = padder.transform(error).unwrap();

        let critical = LogInfo::new("critical", "Critical issue");
        let result_critical = padder.transform(critical).unwrap();

        assert_eq!(result_info.message, "-----Custom filler message");
        assert_eq!(result_error.message, "----Error message");
        assert_eq!(result_critical.message, "-Critical issue");
    }
}
