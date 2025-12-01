use std::collections::hash_map::Iter;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct LoggerLevels {
    levels: HashMap<String, u8>,
}

impl LoggerLevels {
    pub fn new<K: Into<String>>(levels: impl IntoIterator<Item = (K, u8)>) -> Self {
        LoggerLevels {
            levels: levels.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }

    pub fn get_severity(&self, key: &str) -> Option<u8> {
        self.levels.get(key).copied()
    }

    /*/// Returns an iterator over (level_name, severity) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &u8)> {
        self.levels.iter()
    }*/
}

impl IntoIterator for LoggerLevels {
    type Item = (String, u8);
    type IntoIter = std::collections::hash_map::IntoIter<String, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.levels.into_iter()
    }
}

impl<'a> IntoIterator for &'a LoggerLevels {
    type Item = (&'a String, &'a u8);
    type IntoIter = Iter<'a, String, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.levels.iter()
    }
}

impl Default for LoggerLevels {
    fn default() -> Self {
        LoggerLevels::new([
            ("error", 0),
            ("warn", 1),
            ("info", 2),
            ("debug", 3),
            ("trace", 4),
        ])
    }
}

impl From<LoggerLevels> for HashMap<String, u8> {
    fn from(logger_levels: LoggerLevels) -> Self {
        logger_levels.levels
    }
}
