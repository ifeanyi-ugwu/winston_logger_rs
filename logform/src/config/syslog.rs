use std::collections::HashMap;

pub fn levels() -> HashMap<String, usize> {
    HashMap::from([
        ("emerg".to_string(), 0),
        ("alert".to_string(), 1),
        ("crit".to_string(), 2),
        ("error".to_string(), 3),
        ("warning".to_string(), 4),
        ("notice".to_string(), 5),
        ("info".to_string(), 6),
        ("debug".to_string(), 7),
    ])
}

pub fn colors() -> HashMap<String, String> {
    HashMap::from([
        ("emerg".to_string(), "red".to_string()),
        ("alert".to_string(), "yellow".to_string()),
        ("crit".to_string(), "red".to_string()),
        ("error".to_string(), "red".to_string()),
        ("warning".to_string(), "red".to_string()),
        ("notice".to_string(), "yellow".to_string()),
        ("info".to_string(), "green".to_string()),
        ("debug".to_string(), "blue".to_string()),
    ])
}
