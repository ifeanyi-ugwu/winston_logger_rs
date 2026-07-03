#[macro_export]
macro_rules! log {
    // First case: No logger, simple logging
    ($level:ident, $message:expr $(, $key:ident = $value:expr)* $(,)?) => {{
        if $crate::is_level_enabled(stringify!($level)) {
            let entry = $crate::format::LogInfo::new(stringify!($level), $message)
                $(.with_meta(stringify!($key), $value))*;
            $crate::log(entry);
        }
    }};

    // Second case: With logger and key-value metadata
    ($logger:expr, $level:ident, $message:expr $(, $key:ident = $value:expr)* $(,)?) => {{
        if $logger.is_level_enabled(stringify!($level)) {
            let entry = $crate::format::LogInfo::new(stringify!($level), $message)
                $(.with_meta(stringify!($key), $value))*;
            $logger.log(entry);
        }
    }};

    // Third case: With logger and metadata as an expression (e.g., meta!(key1 = value1, key2 = value2))
    ($logger:expr, $level:ident, $message:expr, $meta:expr) => {{
        if $logger.is_level_enabled(stringify!($level)) {
            let entry = $crate::format::LogInfo::new(stringify!($level), $message);
            let entry = $meta.into_iter().fold(entry, |acc, (key, value)| acc.with_meta(key, value));
            $logger.log(entry);
        }
    }};

    // Fourth case: No logger and with metadata as an expression (e.g., meta!(key1 = value1, key2 = value2))
    ($level:ident, $message:expr, $meta:expr) => {{
        if $crate::is_level_enabled(stringify!($level)) {
            let entry = $crate::format::LogInfo::new(stringify!($level), $message);
            let entry = $meta.into_iter().fold(entry, |acc, (key, value)| acc.with_meta(key, value));
            $crate::log(entry);
        }
    }};
}

/// Async twin of [`log!`], for use on an async runtime. Each arm mirrors
/// [`log!`] but dispatches through `log_async`, yielding the task under a
/// saturated `Block` slot instead of parking the thread.
///
/// It expands to an `async {}` block, so the suspend point stays **visible** at
/// the call site — invoke it with `.await`:
///
/// ```ignore
/// log_async!(logger, info, "saved", user = id).await;   // explicit logger
/// log_async!(info, "saved").await;                       // global logger
/// ```
///
/// The level-gate is lazy (nothing is built when the level is off), and the
/// entry never crosses an `.await` before it is enqueued. Opt-in behind the
/// `async-log` feature; see [`Logger::log_async`](crate::Logger::log_async) for
/// the cancellation contract.
#[cfg(feature = "async-log")]
#[macro_export]
macro_rules! log_async {
    // Global logger, key-value metadata.
    ($level:ident, $message:expr $(, $key:ident = $value:expr)* $(,)?) => {{
        async {
            if $crate::is_level_enabled(stringify!($level)) {
                let entry = $crate::format::LogInfo::new(stringify!($level), $message)
                    $(.with_meta(stringify!($key), $value))*;
                $crate::log_async(entry).await;
            }
        }
    }};

    // Explicit logger, key-value metadata.
    ($logger:expr, $level:ident, $message:expr $(, $key:ident = $value:expr)* $(,)?) => {{
        async {
            if $logger.is_level_enabled(stringify!($level)) {
                let entry = $crate::format::LogInfo::new(stringify!($level), $message)
                    $(.with_meta(stringify!($key), $value))*;
                $logger.log_async(entry).await;
            }
        }
    }};

    // Explicit logger, metadata as an expression (e.g. `meta!(...)`).
    ($logger:expr, $level:ident, $message:expr, $meta:expr) => {{
        async {
            if $logger.is_level_enabled(stringify!($level)) {
                let entry = $crate::format::LogInfo::new(stringify!($level), $message);
                let entry = $meta.into_iter().fold(entry, |acc, (key, value)| acc.with_meta(key, value));
                $logger.log_async(entry).await;
            }
        }
    }};

    // Global logger, metadata as an expression.
    ($level:ident, $message:expr, $meta:expr) => {{
        async {
            if $crate::is_level_enabled(stringify!($level)) {
                let entry = $crate::format::LogInfo::new(stringify!($level), $message);
                let entry = $meta.into_iter().fold(entry, |acc, (key, value)| acc.with_meta(key, value));
                $crate::log_async(entry).await;
            }
        }
    }};
}

#[macro_export]
macro_rules! meta {
    ($($key:ident = $value:expr),+ $(,)?) => {{
        vec![
            $(
                (stringify!($key), serde_json::to_value($value).unwrap())
            ),+
        ]
    }}
}

#[macro_export]
macro_rules! create_log_methods {
    ($($level:ident),*) => {
        pub trait LoggerMethods {
            $(
                fn $level(&self, message: &str, metadata: Option<Vec<(&'static str, serde_json::Value)>>);
            )*
        }

        impl LoggerMethods for $crate::Logger {
            $(
                fn $level(&self, message: &str, metadata: Option<Vec<(&'static str, serde_json::Value)>>) {
                    if self.is_level_enabled(stringify!($level)) {
                        let mut entry = $crate::format::LogInfo::new(stringify!($level), message);
                        if let Some(meta) = metadata {
                            for (key, value) in meta {
                                entry = entry.with_meta(key, value);
                            }
                        }
                        self.log(entry);
                    }
                }
            )*
        }
    };
}

#[macro_export]
macro_rules! create_level_macros {
    ($($level:ident),*) => {
        $(
            macro_rules! $level {
                // using the @global is unclean, this would still allow them pass in string literals naturally whilst keeping the @global arm for flexibility of passing the message via an expression
                ($message:literal, $meta:expr) => {{
                    if $crate::is_level_enabled(stringify!($level)) {
                        let mut entry = $crate::format::LogInfo::new(stringify!($level), $message);
                        for (key, value) in $meta {
                            entry = entry.with_meta(key, value);
                        }
                        $crate::log(entry);
                    }
                }};

                // First arm: Log without metadata
                ($logger:expr, $message:expr) => {
                    $crate::log!($logger, $level, $message);
                };

                // Second arm: Log with metadata
                ($logger:expr, $message:expr, $meta:expr) => {{
                    if $logger.is_level_enabled(stringify!($level)) {
                        let mut entry = $crate::format::LogInfo::new(stringify!($level), $message);
                        for (key, value) in $meta {
                            entry = entry.with_meta(key, value);
                        }
                        $logger.log(entry);
                    }
                }};

                // Third arm: Log without metadata using the global logger
                ($message:expr) => {
                    $crate::log!($level, $message);
                };

                // Fourth arm: Log with metadata using the global logger
                // Modified to use a special marker to distinguish from the first arm
               (@global, $message:expr, $meta:expr) => {{
                    if $crate::is_level_enabled(stringify!($level)) {
                        let mut entry = $crate::format::LogInfo::new(stringify!($level), $message);
                        for (key, value) in $meta {
                            entry = entry.with_meta(key, value);
                        }
                        $crate::log(entry);
                    }
                }};
            }
        )*
    };
}

/// Async twin of [`create_level_macros!`]: for each `$level`, generates a
/// `${level}_async!` macro (e.g. `info` → `info_async!`) that dispatches through
/// [`log_async!`]. Each generated macro expands to an awaited `async {}` block —
/// call it with `.await`. Opt-in behind the `async-log` feature.
///
/// ```ignore
/// winston::create_async_level_macros!(error, warn, info, debug, trace);
/// // then, on an async runtime:
/// info_async!(logger, "ready").await;
/// warn_async!("degraded", region = r).await;   // global logger
/// ```
#[cfg(feature = "async-log")]
#[macro_export]
macro_rules! create_async_level_macros {
    ($($level:ident),* $(,)?) => {
        $(
            $crate::paste::paste! {
                #[macro_export]
                macro_rules! [<$level _async>] {
                    // Explicit logger, no metadata.
                    ($logger:expr, $message:expr) => {
                        $crate::log_async!($logger, $level, $message)
                    };
                    // Explicit logger, metadata expression.
                    ($logger:expr, $message:expr, $meta:expr) => {
                        $crate::log_async!($logger, $level, $message, $meta)
                    };
                    // Global logger, no metadata.
                    ($message:expr) => {
                        $crate::log_async!($level, $message)
                    };
                    // Global logger, metadata expression.
                    (@global, $message:expr, $meta:expr) => {
                        $crate::log_async!($level, $message, $meta)
                    };
                }
            }
        )*
    };
}
