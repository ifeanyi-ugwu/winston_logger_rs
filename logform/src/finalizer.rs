use crate::{Format, LogInfo};
use std::fmt;
use std::sync::Arc;

/// What a transport's `write` receives. Carries the finalized [`LogInfo`] and,
/// for string sinks, the rendered terminal string. A structured sink reads
/// `info` and ignores `rendered`; a string sink uses [`Display`](fmt::Display),
/// which yields `rendered` when present and otherwise the structured rendering
/// of `info`.
#[derive(Debug, Clone)]
pub struct FormattedEntry {
    pub info: LogInfo,
    pub rendered: Option<String>,
}

impl FormattedEntry {
    pub fn new(info: LogInfo, rendered: Option<String>) -> Self {
        Self { info, rendered }
    }
}

impl fmt::Display for FormattedEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.rendered {
            Some(rendered) => f.write_str(rendered),
            None => write!(f, "{}", self.info),
        }
    }
}

/// Terminal stage of a format: renders a [`LogInfo`] to the string a string
/// sink writes. Distinct from [`Format`] (a transform, `LogInfo -> LogInfo`)
/// because a finalizer's output type is a `String`, not another `LogInfo`.
///
/// A structured sink has no finalizer and never pays to render. The built-in
/// finalizers are [`json`](crate::json), [`simple`](crate::simple),
/// [`printf`](crate::printf), [`logstash`](crate::logstash),
/// [`pretty_print`](crate::pretty_print), and [`cli`](crate::cli).
pub trait Finalizer: Send + Sync {
    fn finalize(&self, info: &LogInfo) -> Option<String>;
}

/// A complete format: an optional transform chain followed by an optional
/// finalizer. Construct one with [`FinalizeExt::finalize`] (chain + finalizer),
/// [`FinalizeExt::into_pipeline`] (transform-only), or by passing a bare
/// finalizer through [`IntoFormatPipeline`].
pub struct FormatPipeline {
    transforms: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    finalizer: Option<Arc<dyn Finalizer>>,
}

impl FormatPipeline {
    fn from_finalizer(finalizer: Arc<dyn Finalizer>) -> Self {
        Self {
            transforms: None,
            finalizer: Some(finalizer),
        }
    }

    /// Run the transform chain (if any), then the finalizer (if any), yielding
    /// the [`FormattedEntry`] a slot delivers to its transport. Returns `None`
    /// only when a transform drops the entry. With no finalizer, `rendered` is
    /// `None` and no string is produced — a structured sink pays nothing.
    pub fn apply(&self, info: LogInfo) -> Option<FormattedEntry> {
        let info = match &self.transforms {
            Some(transforms) => transforms.transform(info)?,
            None => info,
        };
        let rendered = self.finalizer.as_ref().and_then(|f| f.finalize(&info));
        Some(FormattedEntry { info, rendered })
    }
}

/// Lift a value into a [`FormatPipeline`]. Implemented for any [`Finalizer`], so
/// a bare finalizer (`json()`, a user finalizer) is a complete format. A
/// transform-only format terminates explicitly via
/// [`FinalizeExt::into_pipeline`]; Rust coherence forbids blanketing this over
/// both `Format` and `Finalizer`, so the finalizer side takes the implicit path.
pub trait IntoFormatPipeline {
    fn into_format_pipeline(self) -> FormatPipeline;
}

impl IntoFormatPipeline for FormatPipeline {
    fn into_format_pipeline(self) -> FormatPipeline {
        self
    }
}

impl<F: Finalizer + 'static> IntoFormatPipeline for F {
    fn into_format_pipeline(self) -> FormatPipeline {
        FormatPipeline::from_finalizer(Arc::new(self))
    }
}

/// Terminates a transform chain. Blanket-implemented for every
/// `Format<Input = LogInfo>`, so any transform — built-in or user — composes
/// with any finalizer.
pub trait FinalizeExt: Format<Input = LogInfo> + Sized + Send + Sync + 'static {
    /// End the chain with `finalizer`: `timestamp().finalize(json())`.
    fn finalize<F: Finalizer + 'static>(self, finalizer: F) -> FormatPipeline {
        FormatPipeline {
            transforms: Some(Arc::new(self)),
            finalizer: Some(Arc::new(finalizer)),
        }
    }

    /// Use the chain as a complete format with no finalizer (the rendered
    /// string is `None`; string sinks fall back to their default rendering).
    fn into_pipeline(self) -> FormatPipeline {
        FormatPipeline {
            transforms: Some(Arc::new(self)),
            finalizer: None,
        }
    }
}

impl<T: Format<Input = LogInfo> + Send + Sync + 'static> FinalizeExt for T {}
