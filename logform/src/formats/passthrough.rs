use crate::{Format, LogInfo};

/// A format that passes through LogInfo unchanged.
/// Useful for testing or when you want raw log objects.
#[derive(Debug, Clone, Default)]
pub struct PassthroughFormat;

impl PassthroughFormat {
    pub fn new() -> Self {
        Self
    }
}

//TODO: make format take an input and output
/*impl Format for PassthroughFormat {
    type Input = LogInfo;
    type Output = LogInfo;

    fn transform(&self, info: Self::Input) -> Option<Self::Output> {
        Some(info)
    }
}*/

impl Format for PassthroughFormat {
    type Input = LogInfo;

    fn transform(&self, info: Self::Input) -> Option<Self::Input> {
        Some(info)
    }
}

pub fn passthrough() -> PassthroughFormat {
    PassthroughFormat
}
