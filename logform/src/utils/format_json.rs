use colored::*;
use serde_json::Value;

pub fn format_json_consistently(value: &Value, indent: usize, colorize: bool) -> String {
    let indent_str = " ".repeat(indent);
    match value {
        Value::String(s) => {
            if colorize {
                format!("'{}'", s.green())
            } else {
                format!("'{}'", s)
            }
        }
        Value::Number(n) => {
            if colorize {
                n.to_string().blue().to_string()
            } else {
                n.to_string()
            }
        }
        Value::Bool(b) => {
            if colorize {
                b.to_string().yellow().to_string()
            } else {
                b.to_string()
            }
        }
        Value::Null => {
            if colorize {
                "null".red().to_string()
            } else {
                "null".to_string()
            }
        }
        Value::Object(map) => {
            if map.is_empty() {
                "{}".to_string()
            } else {
                let mut result = String::from("{\n");
                for (k, v) in map {
                    let formatted_value = format_json_consistently(v, indent + 2, colorize);
                    result.push_str(&format!(
                        "{}  {}: {},\n",
                        indent_str,
                        k,
                        formatted_value.trim()
                    ));
                }
                result.pop(); // Remove last newline
                result.pop(); // Remove last comma
                result.push_str(&format!("\n{}}}", indent_str));
                result
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                "[]".to_string()
            } else {
                let mut result = String::from("[\n");
                for v in arr {
                    result.push_str(&format!(
                        "{}  {},\n",
                        indent_str,
                        format_json_consistently(v, indent + 2, colorize).trim()
                    ));
                }
                result.pop(); // Remove last newline
                result.pop(); // Remove last comma
                result.push_str(&format!("\n{}]", indent_str));
                result
            }
        }
    }
}

pub fn format_json(json: &Value, colorize: bool) -> String {
    format_json_consistently(json, 0, colorize)
}
