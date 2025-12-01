# winston_transport

A flexible, extensible logging transport library for Rust, designed to provide various transport implementations with support for batching, asynchronous threading, and adapter interoperability.

## Overview

`winston_transport` offers a modular system for logging transports, enabling efficient and customizable log handling. It provides core abstractions and multiple transport implementations, including batch processing and threaded asynchronous logging. Additionally, it includes adapters to seamlessly convert between the `Transport` trait and standard Rust `Write` trait, facilitating integration with existing IO systems.

## Features

- Core `Transport` trait defining the logging interface.
- `BatchedTransport` for efficient batch processing of log messages.
- `ThreadedTransport` for non-blocking, asynchronous logging on background threads.
- Adapters to convert between `Transport` and `Write` traits (both owned and borrowed).
- Support for querying logs via `LogQuery`.
- Configurable batching parameters such as batch size and flush timing.

## Usage

### Creating a Custom Transport

To create a custom transport, implement the `Transport` trait:

```rust
use winston_transport::Transport;
use logform::LogInfo;

struct MyTransport;

impl Transport for MyTransport {
    fn log(&self, info: LogInfo) {
        // Implement your logging logic here
        println!("{}: {}", info.level, info.message);
    }
}
```

### Basic Transport Usage

```rust
use winston_transport::{Transport, threaded_transport::ThreadedTransport};
use logform::LogInfo;

fn main() {
    let my_transport = MyTransport;
    let threaded = ThreadedTransport::new(my_transport);

    threaded.log(LogInfo::new("INFO", "This is a log message"));
    threaded.flush().expect("Failed to flush logs");
}
```

### Using BatchedTransport

```rust
use winston_transport::{batch_transport::{BatchedTransport, BatchConfig}, Transport};
use logform::LogInfo;
use std::time::Duration;

fn main() {
    let base_transport = MyTransport;
    let config = BatchConfig {
        max_batch_size: 50,
        max_batch_time: Duration::from_millis(200),
        flush_on_drop: true,
    };

    let batched = BatchedTransport::with_config(base_transport, config);

    batched.log(LogInfo::new("INFO", "Batched log message"));
    batched.flush().expect("Flush failed");
}
```

### Using Transport Adapters

Convert a `Transport` into a `Write`:

```rust
use winston_transport::transport_adapters::IntoTransportWriter;
use std::io::Write;

fn example<T: winston_transport::Transport + Sized>(transport: T) {
    let mut writer = transport.into_writer();
    writeln!(writer, "Log message via writer").unwrap();
}
```

Convert a `Write` into a `Transport`:

```rust
use winston_transport::transport_adapters::IntoWriterTransport;
use std::io::stdout;

fn example() {
    let stdout = stdout();
    let transport = stdout.into_transport();
    transport.log(logform::LogInfo::new("INFO", "Log via transport"));
}
```

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
}
}
