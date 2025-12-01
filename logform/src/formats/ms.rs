use crate::LogInfo;
use std::sync::Mutex;
use std::time::Instant;

use super::Format;

pub struct MsFormat {
    prev_time: Mutex<Option<Instant>>,
}

impl MsFormat {
    pub fn new() -> Self {
        MsFormat {
            prev_time: Mutex::new(None),
        }
    }
}

impl Format for MsFormat {
    type Input = LogInfo;

    fn transform(&self, mut input: LogInfo) -> Option<Self::Input> {
        let curr = Instant::now();
        let mut prev_time = self.prev_time.lock().ok()?;
        let diff = match *prev_time {
            Some(prev) => curr.duration_since(prev),
            None => std::time::Duration::from_millis(0), // first call → +0ms
        };

        // update stored time
        *prev_time = Some(curr);

        // Add the time difference in milliseconds to the `info` meta
        input
            .meta
            .insert("ms".to_string(), format!("+{}ms", diff.as_millis()).into());

        Some(input)
    }
}

pub fn ms() -> MsFormat {
    MsFormat::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn test_ms_format() {
        let formatter = ms();
        let info = LogInfo::new("info", "Test message");

        // First transformation → expect +0ms
        formatter.transform(info.clone()).unwrap();
        let result1 = formatter.transform(info.clone()).unwrap();
        let ms1 = result1.meta.get("ms").unwrap().as_str().unwrap();
        assert_eq!(ms1, "+0ms");

        // Simulate a delay
        sleep(Duration::from_millis(300));

        // Second transformation → ~300ms
        let result2 = formatter.transform(info.clone()).unwrap();
        let ms2 = result2.meta.get("ms").unwrap().as_str().unwrap();
        let ms2_value: u64 = ms2
            .trim_start_matches('+')
            .trim_end_matches("ms")
            .parse()
            .unwrap();

        assert!(
            (250..350).contains(&ms2_value),
            "Expected ~300ms, but got {}ms",
            ms2_value
        );
    }
}
