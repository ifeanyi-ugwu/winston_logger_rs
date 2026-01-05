use std::collections::HashMap;

pub fn levels() -> HashMap<String, usize> {
    HashMap::from([
        ("error".to_string(), 0),
        ("warn".to_string(), 1),
        ("help".to_string(), 2),
        ("data".to_string(), 3),
        ("info".to_string(), 4),
        ("debug".to_string(), 5),
        ("prompt".to_string(), 6),
        ("verbose".to_string(), 7),
        ("input".to_string(), 8),
        ("silly".to_string(), 9),
    ])
}

pub fn colors() -> HashMap<String, String> {
    HashMap::from([
        ("error".to_string(), "red".to_string()),
        ("warn".to_string(), "yellow".to_string()),
        ("help".to_string(), "cyan".to_string()),
        ("data".to_string(), "grey".to_string()),
        ("info".to_string(), "green".to_string()),
        ("debug".to_string(), "blue".to_string()),
        ("prompt".to_string(), "grey".to_string()),
        ("verbose".to_string(), "cyan".to_string()),
        ("input".to_string(), "grey".to_string()),
        ("silly".to_string(), "magenta".to_string()),
    ])
}
