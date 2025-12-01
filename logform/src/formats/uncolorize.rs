use super::Format;
use crate::LogInfo;
use regex::Regex;

#[derive(Clone)]
pub struct Uncolorize {
    level: bool,
    message: bool,
}

impl Uncolorize {
    pub fn new() -> Self {
        Self {
            level: true,
            message: true,
        }
    }

    pub fn with_level(mut self, strip_level: bool) -> Self {
        self.level = strip_level;
        self
    }

    pub fn with_message(mut self, strip_message: bool) -> Self {
        self.message = strip_message;
        self
    }

    pub fn transform(&self, mut info: LogInfo) -> Option<LogInfo> {
        if self.level {
            info.level = strip_colors(&info.level);
        }
        if self.message {
            info.message = strip_colors(&info.message);
        }
        Some(info)
    }
}

use std::sync::OnceLock;

static STRIP_COLORS_REGEX: OnceLock<Regex> = OnceLock::new();

fn strip_colors(input: &str) -> String {
    let re = STRIP_COLORS_REGEX.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());
    re.replace_all(input, "").to_string()
}

impl Format for Uncolorize {
    type Input = LogInfo;

    fn transform(&self, info: LogInfo) -> Option<Self::Input> {
        self.transform(info)
    }
}

pub fn uncolorize() -> Uncolorize {
    Uncolorize::new()
}

#[cfg(test)]
mod tests {
    use super::super::colorize::colorize;
    use super::*;
    use colored::control::set_override;
    use serde_json::json;

    #[test]
    fn test_uncolorize_formatter() {
        set_override(true);

        let colors = vec![
            ("info".to_string(), json!(["blue"])),
            ("error".to_string(), json!(["red", "bold"])),
        ]
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
        let colorizer = colorize().with_colors(colors).with_all(true);

        let info = LogInfo::new("info", "This is an info message").with_meta("key", "value");
        let colorized_info = colorizer.transform(info).unwrap();

        let uncolorizer = uncolorize();
        let uncolorized_info = uncolorizer.transform(colorized_info.clone()).unwrap();

        assert_eq!(uncolorized_info.level, "info");
        assert_eq!(uncolorized_info.message, "This is an info message");
    }
}
