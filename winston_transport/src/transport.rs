use std::{future::Future, pin::Pin};

use crate::log_query::LogQuery;

pub trait Transport<L> {
    fn log(&self, info: L);

    fn log_batch(&self, logs: Vec<L>) {
        for log_info in logs {
            self.log(log_info);
        }
    }

    fn flush(&self) -> Result<(), String> {
        Ok(())
    }

    fn query(&self, _options: &LogQuery) -> Result<Vec<L>, String> {
        Ok(Vec::new())
    }
}

/// Async counterpart of [`Transport`]. Implement this when the underlying sink
/// is async-native (HTTP client, async DB driver, etc.) and you don't want to
/// block a worker thread on I/O.
///
/// Each method returns a boxed future to keep the trait `dyn`-compatible.
/// The pipeline drives one transport per task and `await`s `log` sequentially,
/// preserving per-transport ordering.
pub trait AsyncTransport<L>: Send + Sync {
    fn log<'s>(&'s self, info: L) -> Pin<Box<dyn Future<Output = ()> + Send + 's>>;

    fn log_batch<'s>(&'s self, logs: Vec<L>) -> Pin<Box<dyn Future<Output = ()> + Send + 's>>
    where
        L: Send + 's,
    {
        Box::pin(async move {
            for info in logs {
                self.log(info).await;
            }
        })
    }

    fn flush<'s>(
        &'s self,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 's>> {
        Box::pin(async { Ok(()) })
    }

    fn query<'s>(
        &'s self,
        _options: &'s LogQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<L>, String>> + Send + 's>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}
