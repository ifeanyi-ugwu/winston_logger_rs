# `logform`

![Crates.io](https://img.shields.io/crates/v/logform)
![Rust](https://img.shields.io/badge/rust-%E2%9C%94-brightgreen)

A flexible log formatting library designed for chaining and composing log transformations in Rust.

## Overview

`logform` provides a powerful, extensible system to transform structured log messages via composable formatters called **Formats**. Each format implements a common `Format` trait, enabling flexible composition and transformation pipelines.

## Quick Start

```rust
use logform::{timestamp, colorize, align, Format, LogInfo};

fn main() {
    // Compose multiple formats using chaining
    let formatter = timestamp()
        .chain(colorize())
        .chain(align());

    let info = LogInfo::new("info", "Hello, logform!");

    // Apply the composed formatter
    if let Some(transformed) = formatter.transform(info) {
        println!("{}", transformed.message);
    }
}
```

## `LogInfo` — Structured Log Data

At the core is the `LogInfo` struct representing a single log message:

```rust
pub struct LogInfo {
    pub level: String,
    pub message: String,
    pub meta: std::collections::HashMap<String, serde_json::Value>,
}
```

### Common usage

- Create a new `LogInfo`:

  ```rust
  let info = LogInfo::new("info", "User logged in");
  ```

- Add metadata fields:

  ```rust
  let info = info.with_meta("user_id", 12345)
                 .with_meta("session_id", "abcde12345");
  ```

- Remove metadata:

  ```rust
  let info = info.without_meta("session_id");
  ```

- Access metadata:

  ```rust
  if let Some(serde_json::Value::Number(id)) = info.meta.get("user_id") {
          // use id...
  }
  ```

## The `Format` Trait

Formats implement the `Format` trait to transform log messages:

```rust
pub trait Format {
    type Input;

    /// Transforms the input log message, returning:
    /// - `Some(LogInfo)` for transformed logs
    /// - `None` to filter out the log (skip it)
    fn transform(&self, input: Self::Input) -> Option<LogInfo>;

    /// Chain two formats, applying one after another
    fn chain<F>(self, next: F) -> ChainedFormat<Self, F>
    where
        Self: Sized,
        F: Format<Input = Self::Input>,
    {
        ChainedFormat { first: self, next }
    }
}
```

- **Transform:** Modify or produce a new log from input.
- **Filter (return `None`):** Skip processing or output for certain logs.
- **Chaining:** Compose formats easily in sequence.

## Composing Formats

You can chain multiple formats using the `chain` method:

```rust
let combined = timestamp().chain(json()).chain(colorize());
```

Or use the `chain!` macro for succinct chaining of multiple formats:

```rust
use logform::chain;

let combined = chain!(timestamp(), json(), colorize());
```

Chaining stops early when any format returns `None` (useful for filtering logs).

## Available Formats

### `timestamp`

Adds a timestamp to the log metadata.

**Builder methods:**

- `.with_format(&str)` — Customize timestamp display format (uses chrono formatting).
- `.with_alias(&str)` — Add an alias field for the timestamp.

```rust
let ts = timestamp()
    .with_format("%Y-%m-%d %H:%M:%S")
    .with_alias("time");
```

### `simple`

A minimal text formatter producing output like:

```text
level:    message { ...metadata... }
```

Respects padding stored in meta under `"padding"` to align levels nicely.

### `json`

Serializes the log info into a JSON string:

```json
{ "level": "info", "message": "User logged in", "user_id": 12345 }
```

### `align`

Adds a tab character before the message, useful for aligned output.

### `cli`

Combines colorizing and padding:

- Colors the level and/or message.
- Pads messages for neat CLI output.
- Configurable via builder methods like `.with_levels()`, `.with_colors()`, `.with_filler()`, and `.with_all()`.

Example:

```rust
let cli_format = cli()
    .with_filler("*")
    .with_all(true);

let out = cli_format.transform(info).unwrap();
```

### `colorize`

Provides colorization for levels and messages via `colored` crate.

Configurable options include:

- `.with_all(bool)`
- `.with_level(bool)`
- `.with_message(bool)`
- `.with_colors(...)` to specify colors for levels.

### `uncolorize`

Strips ANSI color codes from level and/or message.

### `label`

Adds a label either as a prefix to the message or into metadata.

Builder:

- `.with_label("MY_LABEL")`
- `.with_message(true|false)` — if true, prefix message; else add to meta.

### `logstash`

Transforms the log info into a Logstash-compatible JSON string with fields like `@timestamp`, `@message`, and `@fields`.

### `metadata`

Collects metadata keys into a single key.

Builder methods:

- `.with_key(&str)` — metadata container key (default: `"metadata"`).
- `.with_fill_except(Vec<&str>)` — exclude keys.
- `.with_fill_with(Vec<&str>)` — include only these keys.

### `ms`

Adds time elapsed since the previous log message in milliseconds in the meta key `"ms"`.

### `pad_levels`

Pads the message to align levels uniformly.

Configurable:

- `.with_levels(...)`
- `.with_filler(...)`

### `pretty_print`

Prettifies log output in a human-friendly format, optionally colorized.

Builder:

- `.with_colorize(bool)`

### `printf`

Customize the output with any formatting closure:

```rust
let printf_format = printf(|info| {
    format!("{} - {}: {}",
        info.level,
        info.message,
        serde_json::to_string(&info.meta).unwrap()
    )
});
```

## Filtering Logs

A format can filter out unwanted logs by returning `None` from `transform`.

Example:

```rust
struct IgnorePrivate;

impl Format for IgnorePrivate {
    type Input = LogInfo;

    fn transform(&self, info: LogInfo) -> Option<LogInfo> {
        if let Some(private) = info.meta.get("private") {
            use serde_json::Value;
            if matches!(private, Value::Bool(true)) || private == "true" {
                return None;
            }
        }
        Some(info)
    }
}
```

When chained, subsequent formats will not run if any upstream returns `None`.

## Extending `logform`

Implement `Format` for custom transformations over any input type:

```rust
struct UpperCase;

impl Format for UpperCase {
    type Input = String;

    fn transform(&self, input: String) -> Option<String> {
        if input.is_empty() {
            None
        } else {
            Some(input.to_uppercase())
        }
    }
}
```

Then chain with other formats for composable log processing.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
logform = "0.5"
```

Or use cargo:

```bash
cargo add logform
```

## License

This project is licensed under the MIT License.
