use super::{colorize::Colorizer, pad_levels::Padder, Format};
use crate::{config, LogInfo};
use std::collections::HashSet;

#[derive(Clone)]
pub struct CliFormat {
    colorizer: Colorizer,
    padder: Padder,
}

impl Default for CliFormat {
    fn default() -> Self {
        Self::new()
    }
}

impl CliFormat {
    pub fn new() -> Self {
        let levels: HashSet<String> = config::cli::levels().into_keys().collect();
        let padder = Padder::new().with_levels(levels);
        let colorizer = Colorizer::new();

        CliFormat { colorizer, padder }
    }

    pub fn with_levels(mut self, levels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.padder = self.padder.with_levels(levels);
        self
    }

    pub fn with_filler(mut self, filler: String) -> Self {
        self.padder = self.padder.with_filler(filler);
        self
    }

    pub fn with_all(mut self, all: bool) -> Self {
        self.colorizer = self.colorizer.with_all(all);
        self
    }

    pub fn with_level(mut self, level: bool) -> Self {
        self.colorizer = self.colorizer.with_level(level);
        self
    }

    pub fn with_message(mut self, message: bool) -> Self {
        self.colorizer = self.colorizer.with_message(message);
        self
    }

    pub fn with_colors<T: IntoIterator<Item = (String, serde_json::Value)>>(
        mut self,
        colors: T,
    ) -> Self {
        self.colorizer = self.colorizer.with_colors(colors);
        self
    }

    pub fn with_color(mut self, level: &str, color: serde_json::Value) -> Self {
        self.colorizer = self.colorizer.with_color(level, color);
        self
    }

    fn transform(&self, info: LogInfo) -> Option<LogInfo> {
        let mut transformed_info = self.padder.transform(info)?;
        transformed_info = self.colorizer.transform(transformed_info)?;

        transformed_info.message =
            format!("{}:{}", transformed_info.level, transformed_info.message);

        Some(transformed_info)
    }
}

impl Format for CliFormat {
    type Input = LogInfo;

    fn transform(&self, info: LogInfo) -> Option<Self::Input> {
        self.transform(info)
    }
}

pub fn cli() -> CliFormat {
    CliFormat::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use colored::control::set_override;
    use std::collections::HashMap;

    #[test]
    fn test_cli_format() {
        set_override(true);

        let levels = HashMap::from([
            ("info".to_string(), "info".to_string()),
            ("error".to_string(), "error".to_string()),
        ]);

        let cli_format = CliFormat::new().with_levels(levels.keys());

        let log_info = LogInfo::new("error", "Test message");
        let transformed = cli_format.transform(log_info).unwrap();

        assert_eq!(
            transformed.message,
            format!("\x1b[31merror\x1b[0m: Test message")
        );
    }

    #[test]
    fn test_cli_format_with_options() {
        set_override(true);

        let levels = HashMap::from([
            ("info".to_string(), "info".to_string()),
            ("error".to_string(), "error".to_string()),
        ]);

        let colors = serde_json::json!({
            "info": "blue",
            "error": ["red", "bold"]
        })
        .as_object()
        .unwrap()
        .clone();

        let cli_format = CliFormat::new()
            .with_levels(levels.keys())
            .with_filler("*".to_string())
            .with_all(true)
            .with_colors(colors);

        let log_info = LogInfo::new("error", "Test message");
        let transformed = cli_format.transform(log_info).unwrap();
        assert_eq!(
            transformed.message,
            format!("\x1b[1;31merror\x1b[0m:\x1b[1;31m*Test message\x1b[0m")
        );

        let log_info = LogInfo::new("info", "Another test message");
        let transformed = cli_format.transform(log_info).unwrap();
        assert_eq!(
            transformed.message,
            format!("\x1b[34minfo\x1b[0m:\x1b[34m**Another test message\x1b[0m")
        );
    }
}
