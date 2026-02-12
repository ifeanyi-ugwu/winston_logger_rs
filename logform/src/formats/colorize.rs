use super::Format;
use crate::{config, LogInfo};
use colored::*;
use std::{collections::HashMap, sync::Once};

#[derive(Clone, Debug)]
enum MixedColorType {
    Single(String),
    Multiple(Vec<String>),
}

impl MixedColorType {
    fn as_vec(&self) -> Vec<String> {
        match self {
            MixedColorType::Single(color) => vec![color.clone()],
            MixedColorType::Multiple(colors) => colors.clone(),
        }
    }
}

impl From<String> for MixedColorType {
    fn from(value: String) -> Self {
        MixedColorType::Single(value)
    }
}

impl From<Vec<String>> for MixedColorType {
    fn from(values: Vec<String>) -> Self {
        MixedColorType::Multiple(values)
    }
}

static INIT: Once = Once::new();

#[derive(Clone)]
pub struct Colorizer {
    all_colors: HashMap<String, MixedColorType>,
    all: bool,
    level: bool,
    message: bool,
}

impl Default for Colorizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Colorizer {
    fn init_colors() {
        INIT.call_once(|| {
            colored::control::set_override(true);
        });
    }

    pub fn new() -> Self {
        Self::init_colors();

        let all_colors = config::rust::colors()
            .into_iter()
            .map(|(key, value)| (key, value.into()))
            .collect();

        Self {
            all_colors,
            all: false,
            level: true,
            message: false,
        }
    }

    pub fn with_all(mut self, all: bool) -> Self {
        self.all = all;
        self
    }

    pub fn with_level(mut self, level: bool) -> Self {
        self.level = level;
        self
    }

    pub fn with_message(mut self, message: bool) -> Self {
        self.message = message;
        self
    }

    pub fn with_colors<T: IntoIterator<Item = (String, serde_json::Value)>>(
        mut self,
        colors: T,
    ) -> Self {
        Colorizer::add_colors(&mut self.all_colors, colors);
        self
    }

    // Helper method to add a single color
    pub fn with_color(mut self, level: &str, color: serde_json::Value) -> Self {
        let mut colors = HashMap::new();
        colors.insert(level.to_string(), color);
        Colorizer::add_colors(&mut self.all_colors, colors);
        self
    }

    fn add_colors<T>(all_colors: &mut HashMap<String, MixedColorType>, colors: T)
    where
        T: IntoIterator<Item = (String, serde_json::Value)>,
    {
        for (level, color_val) in colors {
            let color_entry: MixedColorType = match color_val {
                serde_json::Value::String(color_str) => color_str.into(),
                serde_json::Value::Array(color_arr) => color_arr
                    .into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .into(),
                _ => {
                    eprintln!("[logform::colorize] Warning: Invalid color configuration for level '{}': {:?}. Skipping.", level, color_val);
                    continue;
                }
            };
            all_colors.insert(level, color_entry);
        }
    }

    fn colorize(&self, level: &str, message: &str) -> String {
        if let Some(color_entry) = self.all_colors.get(level) {
            color_entry
                .as_vec()
                .iter()
                .fold(message.normal(), |msg, color| apply_color(msg, color))
                .to_string()
        } else {
            message.to_string()
        }
    }

    fn transform(&self, mut info: LogInfo) -> Option<LogInfo> {
        let original_level = info.level.clone();
        if self.all || self.level {
            info.level = self.colorize(&original_level, &info.level);
        }
        if self.all || self.message {
            info.message = self.colorize(&original_level, &info.message);
        }
        Some(info)
    }
}

impl Format for Colorizer {
    type Input = LogInfo;

    fn transform(&self, info: LogInfo) -> Option<Self::Input> {
        self.transform(info)
    }
}

fn apply_color(message: impl Into<colored::ColoredString>, color: &str) -> colored::ColoredString {
    let message = message.into();
    match color {
        "black" => message.black(),
        "red" => message.red(),
        "green" => message.green(),
        "yellow" => message.yellow(),
        "blue" => message.blue(),
        "magenta" => message.magenta(),
        "cyan" => message.cyan(),
        "white" => message.white(),
        "bright_black" => message.bright_black(),
        "bright_red" => message.bright_red(),
        "bright_green" => message.bright_green(),
        "bright_yellow" => message.bright_yellow(),
        "bright_blue" => message.bright_blue(),
        "bright_magenta" => message.bright_magenta(),
        "bright_cyan" => message.bright_cyan(),
        "bright_white" => message.bright_white(),
        "on_black" => message.on_black(),
        "on_red" => message.on_red(),
        "on_green" => message.on_green(),
        "on_yellow" => message.on_yellow(),
        "on_blue" => message.on_blue(),
        "on_magenta" => message.on_magenta(),
        "on_cyan" => message.on_cyan(),
        "on_white" => message.on_white(),
        "on_bright_black" => message.on_bright_black(),
        "on_bright_red" => message.on_bright_red(),
        "on_bright_green" => message.on_bright_green(),
        "on_bright_yellow" => message.on_bright_yellow(),
        "on_bright_blue" => message.on_bright_blue(),
        "on_bright_magenta" => message.on_bright_magenta(),
        "on_bright_cyan" => message.on_bright_cyan(),
        "on_bright_white" => message.on_bright_white(),
        "bold" => message.bold(),
        "underline" => message.underline(),
        "italic" => message.italic(),
        "dimmed" => message.dimmed(),
        "reversed" => message.reversed(),
        "blink" => message.blink(),
        "hidden" => message.hidden(),
        "strikethrough" => message.strikethrough(),
        _ => message,
    }
}

pub fn colorize() -> Colorizer {
    Colorizer::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use colored::control::set_override;
    use serde_json::json;

    #[test]
    fn test_colorizer_format() {
        set_override(true);

        let colors = json!({"info": "blue", "error": ["red", "bold"]})
            .as_object()
            .unwrap()
            .clone();

        let colorizer = Colorizer::new()
            .with_all(true)
            .with_color("warning", json!(["yellow", "italic"]))
            .with_colors(colors);

        let info = LogInfo::new("info", "Info message");
        let result = colorizer.transform(info).unwrap();
        assert!(result.level.contains("\x1b["), "Level should be colorized");
        assert!(
            result.message.contains("\x1b["),
            "Message should be colorized"
        );

        let error_info = LogInfo::new("error", "Error message");
        let result_error = colorizer.transform(error_info).unwrap();
        assert!(
            result_error.level.contains("\x1b["),
            "Error level should be colorized"
        );
        assert!(
            result_error.message.contains("\x1b["),
            "Error message should be colorized"
        );

        let warning_info = LogInfo::new("warning", "Warning message");
        let result_warning = colorizer.transform(warning_info).unwrap();
        assert!(
            result_warning.level.contains("\x1b["),
            "Warning level should be colorized"
        );
        assert!(
            result_warning.message.contains("\x1b["),
            "Warning message should be colorized"
        );
    }
}
