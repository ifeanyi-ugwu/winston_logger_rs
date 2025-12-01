use super::Format;
use crate::LogInfo;

pub struct AlignFormat;

impl Format for AlignFormat {
    type Input = LogInfo;

    fn transform(&self, mut info: LogInfo) -> Option<Self::Input> {
        info.message = format!("\t{}", info.message);
        Some(info)
    }
}

pub fn align() -> AlignFormat {
    AlignFormat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_format() {
        let formatter = AlignFormat;

        let info = LogInfo::new("info", "Test message").with_meta("key", "value");

        let result = formatter.transform(info).unwrap();

        assert!(result.message.starts_with('\t'));
        assert_eq!(result.message, "\tTest message");
    }
}
