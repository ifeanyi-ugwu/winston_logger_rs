use crate::{Finalizer, LogInfo};
use std::sync::Arc;

#[derive(Clone)]
pub struct Printf {
    template: Arc<dyn Fn(&LogInfo) -> String + Send + Sync>,
}

impl Printf {
    pub fn new<T>(template_fn: T) -> Self
    where
        T: Fn(&LogInfo) -> String + Send + Sync + 'static,
    {
        Printf {
            template: Arc::new(template_fn),
        }
    }
}

impl Finalizer for Printf {
    fn finalize(&self, info: &LogInfo) -> Option<String> {
        Some((self.template)(info))
    }
}

pub fn printf<T>(template_fn: T) -> Printf
where
    T: Fn(&LogInfo) -> String + Send + Sync + 'static,
{
    Printf::new(template_fn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_printf_formatter() {
        let formatter = printf(|info: &LogInfo| {
            format!(
                "{} - {}: {}",
                info.level,
                info.message,
                serde_json::Value::Object(info.meta.to_json_object())
            )
        });

        let info = LogInfo::new("info", "This is a message").with_meta("key", "value");

        let result = formatter.finalize(&info).unwrap();

        let expected = "info - This is a message: {\"key\":\"value\"}".to_string();
        assert_eq!(result, expected);
    }
}
