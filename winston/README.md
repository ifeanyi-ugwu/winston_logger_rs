# winston

![Crates.io](https://img.shields.io/crates/v/winston)
![Rust](https://img.shields.io/badge/rust-%E2%9C%94-brightgreen)

A fast, flexible logging library for Rust inspired by Winston.js.

## Overview

Winston provides structured logging with composable transports, formats, and levels. Built on async foundations with intelligent backpressure handling, it's designed for both development convenience and production performance.

## Quick Start

### Simple Console Logging

```rust
use winston::{log, Logger, transports::stdout};

fn main() {
    let logger = Logger::builder()
        .level("info")
        .transport(stdout())
        .build();

    winston::init(logger);

    log!(info, "Application started");
    log!(warn, "Low disk space", usage = 92);

    winston::close();
}
```

### Multi-Transport Logger

```rust
use winston::{Logger, log, format::{timestamp, json, chain}, transports::{stdout, File}};

fn main() {
    let logger = Logger::builder()
        .level("debug")
        .format(chain!(timestamp(), json()))
        .transport(stdout())
        .transport(File::builder().filename("app.log").build())
        .build();

    log!(logger, info, "Logging to console and file");
}
```

## Core Concepts

### LogInfo - Structured Log Data

Every log message is represented by a `LogInfo` struct containing level, message, and metadata:

```rust
// Simple log
let info = LogInfo::new("info", "User authenticated");

// With metadata
let info = info.with_meta("user_id", 12345)
               .with_meta("session_id", "abc123");
```

### Transports - Where Logs Go

Transports define output destinations. Each implements the `Transport` trait:

```rust
pub trait Transport: Send + Sync {
    fn log(&self, info: LogInfo);
    fn flush(&self) -> Result<(), String> { Ok(()) }
    fn query(&self, options: &LogQuery) -> Result<Vec<LogInfo>, String> { Ok(Vec::new()) }
}
```

**Built-in transports:**

- `stdout()` / `stderr()` - Console output
- `File` - File logging with querying support
- `WriterTransport` - Generic writer for custom destinations

**Multiple transports example:**

```rust
// Using builder - all transports use logger's global level and format
let logger = Logger::builder()
    .transport(stdout())
    .transport(File::builder().filename("app.log").build())
    .build();

// For custom level/format per transport, you have two options:

// Option 1: Runtime fluent API with logger.transport()
let logger = Logger::new(None);

let console_handle = logger.transport(stdout())
    .with_level("info")
    .with_format(colorize())
    .add();

let file_handle = logger.transport(File::builder().filename("app.log").build())
    .with_level("debug")
    .with_format(json())
    .add();

// Option 2: Pre-configure with LoggerTransport (works both build-time and runtime)
use winston::LoggerTransport;

let console_transport = LoggerTransport::new(stdout())
    .with_level("info")
    .with_format(colorize());

let file_transport = LoggerTransport::new(File::builder().filename("app.log").build())
    .with_level("debug")
    .with_format(json());

// Use in builder (build-time)
let logger = Logger::builder()
    .transport(console_transport.clone())
    .transport(file_transport.clone())
    .build();

// Or add at runtime
let logger = Logger::new(None);
let console_handle = logger.add_transport(console_transport);
let file_handle = logger.add_transport(file_transport);
```

### Levels - Message Priority

Winston uses RFC 5424 severity levels (lower = more critical):

```rust
levels: {
    error: 0,   // System errors
    warn:  1,   // Warnings
    info:  2,   // General info
    debug: 3,   // Debug details
    trace: 4    // Verbose tracing
}
```

Set minimum level to control verbosity:

```rust
let logger = Logger::builder()
    .level("info")  // Logs info, warn, error (filters out debug, trace)
    .build();
```

### Formats - Message Styling

Winston uses the powerful [logform](https://github.com/ifeanyi-ugwu/logform_rs) library for message formatting through composable format chaining:

```rust
use winston::format::{timestamp, json, colorize, chain};

// Using the chain method
let logger = Logger::builder()
    .format(
        timestamp()
            .with_format("%Y-%m-%d %H:%M:%S")
            .chain(colorize())
            .chain(json())
    )
    .build();

// Using the chain! macro for cleaner syntax
let logger = Logger::builder()
    .format(chain!(
        timestamp().with_format("%Y-%m-%d %H:%M:%S"),
        colorize(),
        json()
    ))
    .build();
```

**Per-transport formatting:**

```rust
let logger = Logger::builder()
    .transport(stdout())  // Uses logger's global format
    .build();

// Or configure per-transport
let logger = Logger::new(None);

logger.transport(stdout())
    .with_format(chain!(
        timestamp().with_format("%H:%M:%S"),
        colorize()
    ))
    .add();

logger.transport(File::builder().filename("app.log").build())
    .with_format(chain!(
        timestamp().with_format("%Y-%m-%d %H:%M:%S"),
        json()
    ))
    .add();
```

## Advanced Features

### Custom Log Levels

Define domain-specific severity levels:

```rust
use std::collections::HashMap;

let custom_levels = HashMap::from([
    ("critical", 0),
    ("high", 1),
    ("medium", 2),
    ("low", 3)
]);

let logger = Logger::builder()
    .levels(custom_levels)
    .build();
```

Create custom logging methods and macros:

```rust
winston::create_log_methods!(critical, high, medium, low);
winston::create_level_macros!(critical, high, medium, low);

// Now you can use:
logger.critical("System failure", None);
high!(logger, "Priority task failed", retries = 3);
```

### Dynamic Transport Management

Add and remove transports at runtime:

```rust
let logger = Logger::new(None);

// Add transports and get handles
let console_handle = logger.add_transport(stdout());
let file_handle = logger.transport(File::builder().filename("app.log").build())
    .with_level("debug")
    .add();

// Later, remove specific transports
logger.remove_transport(console_handle);  // Stop console logging
logger.remove_transport(file_handle);     // Stop file logging
```

### Backpressure Management

Control behavior when the log buffer fills up:

```rust
use winston::BackpressureStrategy;

let logger = Logger::builder()
    .channel_capacity(1000)
    .backpressure_strategy(BackpressureStrategy::DropOldest)  // or Block, DropCurrent
    .build();
```

**Strategy recommendations:**

- `Block` - Best for critical logs where no messages should be lost
- `DropOldest` - Good for high-volume applications where recent logs matter most
- `DropCurrent` - Suitable when preserving historical context is more important

### Log Querying

Retrieve historical logs from queryable transports:

```rust
use winston::LogQuery;

let query = LogQuery::new()
    .from("2 hours ago")
    .until("now")
    .levels(vec!["error", "warn"])
    .search_term("database")
    .limit(50);

let results = logger.query(query)?;
```

**Query options:**

- `from` / `until` - Time range (supports natural language via `parse_datetime`)
- `levels` - Filter by severity
- `search_term` - Text search in messages
- `limit` / `start` - Pagination
- `order` - `asc` or `desc`
- `fields` - Projection (which fields to return)

### Runtime Reconfiguration

Change logger settings dynamically:

```rust
logger.configure(
    LoggerOptions::new()
        .level("debug")
        .transport(File::builder().filename("debug.log").build())
);
```

### Custom Transports

Implement the `Transport` trait for custom destinations:

```rust
use winston::{Transport, LogInfo};

struct DatabaseTransport {
    connection: DatabaseConnection,
}

impl Transport for DatabaseTransport {
    fn log(&self, info: LogInfo) {
        // Insert log into database
        self.connection.execute("INSERT INTO logs ...", &info);
    }

    fn query(&self, options: &LogQuery) -> Result<Vec<LogInfo>, String> {
        // Query logs from database
        self.connection.query_logs(options)
    }
}
```

## Global vs Instance Logging

### Global Logger (Singleton)

Convenient for application-wide logging:

```rust
use winston::{Logger, log, transports::stdout};

fn main() {
    let logger = Logger::builder()
        .transport(stdout())
        .build();

    winston::init(logger);
    log!(info, "Using global logger");
    winston::flush().unwrap(); // Important: flush before app exit
    winston::close();
}
```

### Logger Instances

Better for libraries or multi-tenant applications:

```rust
let logger = Logger::builder()
    .transport(stdout())
    .build();

log!(logger, info, "Using specific logger instance");
// Automatic cleanup on drop
```

## Performance Tips

1. **Buffer sizing**: Tune `channel_capacity` based on log volume
2. **Transport selection**: File transport is faster than stdout for high-volume logging
3. **Format efficiency**: Simple formats are faster than complex chained formats
4. **Level filtering**: Set appropriate minimum levels to avoid unnecessary processing
5. **Format chaining order**: Place expensive formats (like colorization) last in the chain

## Integration with the `log` Crate

Winston can also act as a backend for the widely used [`log`](https://crates.io/crates/log) facade.  
This means that existing libraries and crates which emit logs via `log` will automatically route their output through Winston's transports and formatting system.

Enable the feature in `Cargo.toml`:

```toml
[dependencies]
winston = { version = "0.5", features = ["log-backend"] }
```

Then initialize Winston as the global logger:

```rust
use winston::{Logger, transports::stdout};

fn main() {
    // Initialize winston
    let logger = Logger::builder()
        .transport(stdout())
        .build();

    winston::init(logger);
    winston::register_with_log().unwrap();

    log::info!("Hello from the log crate!");
    log::warn!("This also goes through Winston transports");

    winston::close();
}
```

Notes:

- Key–value metadata support from log is available with the `log-backend-kv` feature.
- Winston's transports, levels, formats, and backpressure strategies apply seamlessly.
- Useful when integrating Winston into projects that already rely on the log ecosystem.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
winston = "0.5"
```

Or use cargo:

```bash
cargo add winston
```

## Contributing

Contributions welcome! Please submit issues and pull requests on GitHub.

## License

MIT License

## Acknowledgments

Inspired by the excellent [Winston.js](https://github.com/winstonjs/winston) logging library.
