use std::collections::HashMap;

// nmp levels and colors
/*
pub fn levels() -> HashMap<String, usize> {
    let levels = HashMap::from([
        ("error".to_string(), 0),
        ("warn".to_string(), 1),
        ("info".to_string(), 2),
        ("http".to_string(), 3),
        ("verbose".to_string(), 4),
        ("debug".to_string(), 5),
        ("silly".to_string(), 6),
    ]);
    levels
}

pub fn colors() -> HashMap<String, String> {
    let colors = HashMap::from([
        ("error".to_string(), "red".to_string()),
        ("warn".to_string(), "yellow".to_string()),
        ("info".to_string(), "green".to_string()),
        ("http".to_string(), "green".to_string()),
        ("verbose".to_string(), "cyan".to_string()),
        ("debug".to_string(), "blue".to_string()),
        ("silly".to_string(), "magenta".to_string()),
    ]);
    colors
}
*/
pub fn levels() -> HashMap<String, usize> {
    HashMap::from([
        ("error".to_string(), 0),
        ("warn".to_string(), 1),
        ("info".to_string(), 2),
        ("debug".to_string(), 3),
        ("trace".to_string(), 4),
    ])
}

pub fn colors() -> HashMap<String, String> {
    HashMap::from([
        ("error".to_string(), "red".to_string()),
        ("warn".to_string(), "yellow".to_string()),
        ("info".to_string(), "green".to_string()),
        ("debug".to_string(), "blue".to_string()),
        ("trace".to_string(), "magenta".to_string()),
    ])
}
