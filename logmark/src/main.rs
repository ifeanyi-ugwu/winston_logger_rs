use chrono::Local;
use env_logger;
use env_logger::fmt::Formatter;
use log::info;
use rand::seq::SliceRandom;
use rand::thread_rng;
use slog::{o, Drain, Logger, KV};
use slog_async;
use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::process::{exit, Command};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Instant;
use std::{env, fs, thread};
use tabled::{builder::Builder, settings::Style};
use tracing::{event, Level};
use winston::format::Format as _;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Returns the number of bytes currently allocated on the heap according to
/// jemalloc. Advances the stats epoch first so the read is current.
fn jemalloc_allocated() -> usize {
    tikv_jemalloc_ctl::epoch::mib().unwrap().advance().unwrap();
    tikv_jemalloc_ctl::stats::allocated::mib()
        .unwrap()
        .read()
        .unwrap()
}

const ITERATIONS: u32 = 100_000;
const LATENCY_ITERATIONS: usize = 10_000;
const NUM_RUNS: usize = 3;
const NUM_WARMUP_RUNS: usize = 1;
const MESSAGE: &str = "A logging message that is reasonably long";
const CONCURRENT_THREADS: usize = 4;
const CONC_ITERS_PER_THREAD: usize = 25_000;
// 10% of the total concurrent log volume — guarantees OverflowStrategy::Block fires repeatedly.
const SATURATION_CHANNEL_SIZE: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OutputTarget {
    Sink,
    Stdout,
    File,
}

impl OutputTarget {
    fn as_str(&self) -> &'static str {
        match self {
            OutputTarget::Sink => "sink",
            OutputTarget::Stdout => "stdout",
            OutputTarget::File => "file",
        }
    }

    fn all() -> Vec<OutputTarget> {
        vec![OutputTarget::Sink, OutputTarget::Stdout, OutputTarget::File]
    }
}

struct BenchmarkResult {
    name: String,
    elapsed: f64,
    ops: f64,
    memory_usage: f64,
    startup_secs: f64,
    drain_secs: f64,
    target: OutputTarget,
    p50_ns: u64,
    p99_ns: u64,
    p999_ns: u64,
    max_ns: u64,
}

#[derive(Default)]
struct ConcurrentStats {
    ops_rates: Vec<f64>,
    p99_ns: Vec<u64>,
}

#[derive(Default)]
struct BenchmarkStats {
    elapsed_times: Vec<f64>,
    ops_rates: Vec<f64>,
    memory_usages: Vec<f64>,
    startup_times: Vec<f64>,
    drain_times: Vec<f64>,
    p50_ns: Vec<u64>,
    p99_ns: Vec<u64>,
    p999_ns: Vec<u64>,
    max_ns: Vec<u64>,
}

/// Computes P50 / P99 / P99.9 / max from a pre-sorted nanosecond sample slice.
fn latency_percentiles(sorted: &[u64]) -> (u64, u64, u64, u64) {
    let n = sorted.len();
    (
        sorted[n / 2],
        sorted[n * 99 / 100],
        sorted[(n * 999 / 1000).min(n - 1)],
        *sorted.last().unwrap(),
    )
}

fn run_benchmark<F: Fn()>(
    name: &str,
    target: OutputTarget,
    startup_secs: f64,
    bench_fn: F,
) -> BenchmarkResult {
    // Latency pass: 10K timed individual calls, results discarded for throughput stats.
    let mut samples = vec![0u64; LATENCY_ITERATIONS];
    for slot in samples.iter_mut() {
        let t = Instant::now();
        bench_fn();
        *slot = t.elapsed().as_nanos() as u64;
    }
    samples.sort_unstable();
    let (p50, p99, p999, max) = latency_percentiles(&samples);

    // Throughput pass: 100K calls, wall-clock only.
    let before = jemalloc_allocated();
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        bench_fn();
    }
    let elapsed = start.elapsed();
    let after = jemalloc_allocated();

    BenchmarkResult {
        name: name.to_string(),
        target,
        elapsed: elapsed.as_secs_f64(),
        ops: ITERATIONS as f64 / elapsed.as_secs_f64(),
        memory_usage: after.saturating_sub(before) as f64 / (1024.0 * 1024.0),
        startup_secs,
        drain_secs: 0.0,
        p50_ns: p50,
        p99_ns: p99,
        p999_ns: p999,
        max_ns: max,
    }
}

// Runs bench_fn across CONCURRENT_THREADS threads, all starting at the same
// barrier. Returns (total_ops_per_sec, worst_thread_P99_ns).
fn run_concurrent<S, F>(setup: S) -> (f64, u64)
where
    S: Fn() -> F + Send + Sync + 'static,
    F: Fn() + Send + 'static,
{
    let setup = Arc::new(setup);
    let barrier = Arc::new(Barrier::new(CONCURRENT_THREADS));

    let handles: Vec<_> = (0..CONCURRENT_THREADS)
        .map(|_| {
            let setup = Arc::clone(&setup);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let bench_fn = setup();
                barrier.wait();
                let mut samples = vec![0u64; CONC_ITERS_PER_THREAD];
                let start = Instant::now();
                for slot in samples.iter_mut() {
                    let t = Instant::now();
                    bench_fn();
                    *slot = t.elapsed().as_nanos() as u64;
                }
                let elapsed = start.elapsed().as_secs_f64();
                samples.sort_unstable();
                (elapsed, samples[CONC_ITERS_PER_THREAD * 99 / 100])
            })
        })
        .collect();

    let results: Vec<(f64, u64)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wall = results.iter().map(|(e, _)| *e).fold(0.0f64, f64::max);
    let total = (CONCURRENT_THREADS * CONC_ITERS_PER_THREAD) as f64;
    let p99 = results.iter().map(|(_, p)| *p).max().unwrap_or(0);
    (total / wall, p99)
}

fn bench_env_logger(target: OutputTarget) -> BenchmarkResult {
    env::set_var("RUST_LOG", "info");

    let mut builder = env_logger::Builder::from_default_env();
    builder.format(|buf: &mut Formatter, record| {
        writeln!(
            buf,
            "{{\"timestamp\":\"{}\",\"level\":\"{}\",\"target\":\"{}\",\"message\":\"{}\"}}",
            chrono::Utc::now().to_rfc3339(),
            record.level(),
            record.target(),
            record.args()
        )
    });

    let init_start = Instant::now();
    match target {
        OutputTarget::Sink => {
            builder
                .target(env_logger::Target::Pipe(Box::new(std::io::sink())))
                .init();
        }
        OutputTarget::Stdout => {
            builder.target(env_logger::Target::Stdout).init();
        }
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/env_logger.log").unwrap();
            builder
                .target(env_logger::Target::Pipe(Box::new(log_file)))
                .init();
        }
    }
    let startup_secs = init_start.elapsed().as_secs_f64();

    run_benchmark("env_logger", target, startup_secs, || {
        info!("{} {}", MESSAGE, "env_logger");
    })
}

fn bench_fern(target: OutputTarget) -> BenchmarkResult {
    let dispatch = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                r#"{{"timestamp":"{}","level":"{}","target":"{}","message":"{}"}}"#,
                chrono::Utc::now().to_rfc3339(),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info);

    let init_start = Instant::now();
    match target {
        OutputTarget::Sink => dispatch
            .chain(Box::new(std::io::sink()) as Box<dyn std::io::Write + Send>)
            .apply()
            .unwrap(),
        OutputTarget::Stdout => dispatch.chain(std::io::stdout()).apply().unwrap(),
        OutputTarget::File => {
            let f = std::fs::File::create("logs/fern.log").unwrap();
            dispatch.chain(f).apply().unwrap()
        }
    }
    let startup_secs = init_start.elapsed().as_secs_f64();

    run_benchmark("fern", target, startup_secs, || {
        log::info!("{} {}", MESSAGE, "fern");
    })
}

// Serialize to a Vec<u8> first (no lock, no syscall), then write the whole
// buffer in one locked write_all. This pushes the serde_json streaming
// overhead outside the critical section and collapses N file-write syscalls
// per record into one.
struct PrebufDrain<W: Write + Send> {
    writer: Mutex<W>,
}

impl<W: Write + Send> PrebufDrain<W> {
    fn new(w: W) -> Self {
        PrebufDrain {
            writer: Mutex::new(w),
        }
    }
}

impl<W: Write + Send + 'static> slog::Drain for PrebufDrain<W> {
    type Ok = ();
    type Err = slog::Never;

    fn log(&self, record: &slog::Record, values: &slog::OwnedKVList) -> Result<(), slog::Never> {
        struct KvSer<'a>(&'a mut Vec<u8>);
        impl slog::Serializer for KvSer<'_> {
            fn emit_arguments(
                &mut self,
                key: slog::Key,
                val: &std::fmt::Arguments,
            ) -> slog::Result {
                write!(self.0, ",\"{}\":\"{}\"", key, val).ok();
                Ok(())
            }
        }

        let mut buf = Vec::with_capacity(256);
        write!(
            buf,
            "{{\"timestamp\":\"{}\",\"level\":\"{}\",\"target\":\"{}\",\"message\":\"{}\"",
            chrono::Utc::now().to_rfc3339(),
            record.level().as_str(),
            record.location().module,
            record.msg()
        )
        .ok();
        let mut ser = KvSer(&mut buf);
        record.kv().serialize(record, &mut ser).ok();
        values.serialize(record, &mut ser).ok();
        buf.extend_from_slice(b"}\n");

        self.writer.lock().unwrap().write_all(&buf).ok();
        Ok(())
    }
}

fn bench_slog(target: OutputTarget) -> BenchmarkResult {
    let init_start = Instant::now();
    let root = match target {
        OutputTarget::Sink => Logger::root(PrebufDrain::new(std::io::sink()).fuse(), o!()),
        OutputTarget::Stdout => Logger::root(PrebufDrain::new(std::io::stdout()).fuse(), o!()),
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/slog.log").unwrap();
            Logger::root(PrebufDrain::new(log_file).fuse(), o!())
        }
    };
    let startup_secs = init_start.elapsed().as_secs_f64();

    run_benchmark("slog", target, startup_secs, move || {
        slog::info!(root, "{} {}", MESSAGE, "slog");
    })
}

fn bench_slog_async(target: OutputTarget) -> BenchmarkResult {
    const CHANNEL_SIZE: usize = 50_000;

    macro_rules! async_drain {
        ($w:expr) => {
            slog_async::Async::new(PrebufDrain::new($w).fuse())
                .chan_size(CHANNEL_SIZE)
                .overflow_strategy(slog_async::OverflowStrategy::Block)
                .build()
                .fuse()
        };
    }

    let init_start = Instant::now();
    let drain = match target {
        OutputTarget::Sink => async_drain!(std::io::sink()),
        OutputTarget::Stdout => async_drain!(std::io::stdout()),
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/slog_async.log").unwrap();
            async_drain!(log_file)
        }
    };

    let root = Logger::root(drain, o!());
    let startup_secs = init_start.elapsed().as_secs_f64();

    // Latency pass: enqueue latency (caller's view, no drain per call).
    let mut samples = vec![0u64; LATENCY_ITERATIONS];
    for slot in samples.iter_mut() {
        let t = Instant::now();
        slog::info!(root, "{} {}", MESSAGE, "slog_async");
        *slot = t.elapsed().as_nanos() as u64;
    }
    samples.sort_unstable();
    let (p50, p99, p999, max) = latency_percentiles(&samples);

    // Throughput pass: includes drain (worker-thread join) in elapsed.
    let before = jemalloc_allocated();
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        slog::info!(root, "{} {}", MESSAGE, "slog_async");
    }
    let drain_start = Instant::now();
    drop(root);
    let drain_secs = drain_start.elapsed().as_secs_f64();
    let elapsed = start.elapsed();
    let after = jemalloc_allocated();

    BenchmarkResult {
        name: "slog_async".to_string(),
        target,
        elapsed: elapsed.as_secs_f64(),
        ops: ITERATIONS as f64 / elapsed.as_secs_f64(),
        memory_usage: after.saturating_sub(before) as f64 / (1024.0 * 1024.0),
        startup_secs,
        drain_secs,
        p50_ns: p50,
        p99_ns: p99,
        p999_ns: p999,
        max_ns: max,
    }
}

fn bench_tracing(target: OutputTarget) -> BenchmarkResult {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    let writer = match target {
        OutputTarget::Sink => BoxMakeWriter::new(std::io::sink),
        OutputTarget::Stdout => BoxMakeWriter::new(std::io::stdout),
        OutputTarget::File => {
            let file = std::fs::File::create("logs/tracing.log").unwrap();
            BoxMakeWriter::new(move || file.try_clone().unwrap())
        }
    };

    let init_start = Instant::now();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(writer)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting tracing default failed");
    let startup_secs = init_start.elapsed().as_secs_f64();

    run_benchmark("tracing", target, startup_secs, || {
        event!(Level::INFO, "{} {}", MESSAGE, "tracing");
    })
}

fn bench_tracing_async(target: OutputTarget) -> BenchmarkResult {
    use tracing_appender::non_blocking::NonBlockingBuilder;
    use tracing_subscriber::fmt;

    const CHANNEL_SIZE: usize = 50_000;

    let init_start = Instant::now();
    let (writer, guard) = match target {
        OutputTarget::Sink => NonBlockingBuilder::default()
            .buffered_lines_limit(CHANNEL_SIZE)
            .lossy(false)
            .finish(std::io::sink()),
        OutputTarget::Stdout => NonBlockingBuilder::default()
            .buffered_lines_limit(CHANNEL_SIZE)
            .lossy(false)
            .finish(std::io::stdout()),
        OutputTarget::File => {
            let file = std::fs::File::create("logs/tracing_async.log").unwrap();
            NonBlockingBuilder::default()
                .buffered_lines_limit(CHANNEL_SIZE)
                .lossy(false)
                .finish(file)
        }
    };

    let subscriber = fmt()
        .json()
        .flatten_event(true)
        .with_writer(writer)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting tracing default failed");
    let startup_secs = init_start.elapsed().as_secs_f64();

    // Latency pass: enqueue latency (caller's view, no drain per call).
    let mut samples = vec![0u64; LATENCY_ITERATIONS];
    for slot in samples.iter_mut() {
        let t = Instant::now();
        event!(Level::INFO, "{} {}", MESSAGE, "tracing_async");
        *slot = t.elapsed().as_nanos() as u64;
    }
    samples.sort_unstable();
    let (p50, p99, p999, max) = latency_percentiles(&samples);

    // Throughput pass: WorkerGuard::drop blocks until background thread flushes.
    let before = jemalloc_allocated();
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        event!(Level::INFO, "{} {}", MESSAGE, "tracing_async");
    }
    let drain_start = Instant::now();
    drop(guard);
    let drain_secs = drain_start.elapsed().as_secs_f64();
    let elapsed = start.elapsed();
    let after = jemalloc_allocated();

    BenchmarkResult {
        name: "tracing_async".to_string(),
        target,
        elapsed: elapsed.as_secs_f64(),
        ops: ITERATIONS as f64 / elapsed.as_secs_f64(),
        memory_usage: after.saturating_sub(before) as f64 / (1024.0 * 1024.0),
        startup_secs,
        drain_secs,
        p50_ns: p50,
        p99_ns: p99,
        p999_ns: p999,
        max_ns: max,
    }
}

fn bench_winston(target: OutputTarget) -> BenchmarkResult {
    let builder = winston::Logger::builder()
        .channel_capacity(50_000)
        .backpressure_strategy(winston::BackpressureStrategy::Block)
        .format(
            winston::format::timestamp().chain(winston::format::printf(|info| {
                format!(
                    r#"{{"timestamp":"{}","level":"{}","target":"logmark","message":"{}"}}"#,
                    info.meta
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    info.level.to_ascii_uppercase(),
                    info.message
                )
            })),
        );

    let init_start = Instant::now();
    let logger = match target {
        OutputTarget::Sink => builder
            .transport(winston::transports::WriterTransport::new(std::io::sink()))
            .build(),
        OutputTarget::Stdout => builder
            .transport(winston::transports::WriterTransport::new(BufWriter::new(
                std::io::stdout(),
            )))
            .build(),
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/winston.log").unwrap();
            builder
                .transport(winston::transports::WriterTransport::new(BufWriter::new(
                    log_file,
                )))
                .build()
        }
    };
    let startup_secs = init_start.elapsed().as_secs_f64();

    // Latency pass: enqueue latency (caller's view, no drain per call).
    let mut samples = vec![0u64; LATENCY_ITERATIONS];
    for slot in samples.iter_mut() {
        let t = Instant::now();
        winston::log!(logger, info, format!("{} {}", MESSAGE, "winston"));
        *slot = t.elapsed().as_nanos() as u64;
    }
    samples.sort_unstable();
    let (p50, p99, p999, max) = latency_percentiles(&samples);

    // Throughput pass: drop flushes the internal worker channel.
    let before = jemalloc_allocated();
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        winston::log!(logger, info, format!("{} {}", MESSAGE, "winston"));
    }
    let drain_start = Instant::now();
    drop(logger);
    let drain_secs = drain_start.elapsed().as_secs_f64();
    let elapsed = start.elapsed();
    let after = jemalloc_allocated();

    BenchmarkResult {
        name: "winston".to_string(),
        target,
        elapsed: elapsed.as_secs_f64(),
        ops: ITERATIONS as f64 / elapsed.as_secs_f64(),
        memory_usage: after.saturating_sub(before) as f64 / (1024.0 * 1024.0),
        startup_secs,
        drain_secs,
        p50_ns: p50,
        p99_ns: p99,
        p999_ns: p999,
        max_ns: max,
    }
}

fn bench_env_logger_concurrent(target: OutputTarget) -> (f64, u64) {
    env::set_var("RUST_LOG", "info");
    let mut builder = env_logger::Builder::from_default_env();
    builder.format(|buf: &mut Formatter, record| {
        writeln!(
            buf,
            "{{\"timestamp\":\"{}\",\"level\":\"{}\",\"target\":\"{}\",\"message\":\"{}\"}}",
            chrono::Utc::now().to_rfc3339(),
            record.level(),
            record.target(),
            record.args()
        )
    });
    match target {
        OutputTarget::Sink => {
            builder
                .target(env_logger::Target::Pipe(Box::new(std::io::sink())))
                .init();
        }
        OutputTarget::Stdout => {
            builder.target(env_logger::Target::Stdout).init();
        }
        OutputTarget::File => {
            let f = std::fs::File::create("logs/env_logger_conc.log").unwrap();
            builder.target(env_logger::Target::Pipe(Box::new(f))).init();
        }
    }
    run_concurrent(|| || log::info!("{} {}", MESSAGE, "env_logger"))
}

fn bench_fern_concurrent(target: OutputTarget) -> (f64, u64) {
    let dispatch = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                r#"{{"timestamp":"{}","level":"{}","target":"{}","message":"{}"}}"#,
                chrono::Utc::now().to_rfc3339(),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info);
    match target {
        OutputTarget::Sink => dispatch
            .chain(Box::new(std::io::sink()) as Box<dyn std::io::Write + Send>)
            .apply()
            .unwrap(),
        OutputTarget::Stdout => dispatch.chain(std::io::stdout()).apply().unwrap(),
        OutputTarget::File => {
            let f = std::fs::File::create("logs/fern_conc.log").unwrap();
            dispatch.chain(f).apply().unwrap()
        }
    }
    run_concurrent(|| || log::info!("{} {}", MESSAGE, "fern"))
}

fn bench_slog_concurrent(target: OutputTarget) -> (f64, u64) {
    let root = match target {
        OutputTarget::Sink => Logger::root(PrebufDrain::new(std::io::sink()).fuse(), o!()),
        OutputTarget::Stdout => Logger::root(PrebufDrain::new(std::io::stdout()).fuse(), o!()),
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/slog_conc.log").unwrap();
            Logger::root(PrebufDrain::new(log_file).fuse(), o!())
        }
    };
    run_concurrent(move || {
        let root = root.clone();
        move || slog::info!(root, "{} {}", MESSAGE, "slog")
    })
}

fn bench_slog_async_concurrent(target: OutputTarget) -> (f64, u64) {
    const CHANNEL_SIZE: usize = 200_000;
    macro_rules! async_drain {
        ($w:expr) => {
            slog_async::Async::new(PrebufDrain::new($w).fuse())
                .chan_size(CHANNEL_SIZE)
                .overflow_strategy(slog_async::OverflowStrategy::Block)
                .build()
                .fuse()
        };
    }

    let drain = match target {
        OutputTarget::Sink => async_drain!(std::io::sink()),
        OutputTarget::Stdout => async_drain!(std::io::stdout()),
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/slog_async_conc.log").unwrap();
            async_drain!(log_file)
        }
    };
    let root = Logger::root(drain, o!());
    run_concurrent(move || {
        let root = root.clone();
        move || slog::info!(root, "{} {}", MESSAGE, "slog_async")
    })
}

fn bench_tracing_concurrent(target: OutputTarget) -> (f64, u64) {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    let writer = match target {
        OutputTarget::Sink => BoxMakeWriter::new(std::io::sink),
        OutputTarget::Stdout => BoxMakeWriter::new(std::io::stdout),
        OutputTarget::File => {
            let file = std::fs::File::create("logs/tracing_conc.log").unwrap();
            BoxMakeWriter::new(move || file.try_clone().unwrap())
        }
    };
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(writer)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting tracing default failed");
    run_concurrent(|| || event!(Level::INFO, "{} {}", MESSAGE, "tracing"))
}

fn bench_tracing_async_concurrent(target: OutputTarget) -> (f64, u64) {
    use tracing_appender::non_blocking::NonBlockingBuilder;
    use tracing_subscriber::fmt;
    const CHANNEL_SIZE: usize = 200_000;
    let (writer, _guard) = match target {
        OutputTarget::Sink => NonBlockingBuilder::default()
            .buffered_lines_limit(CHANNEL_SIZE)
            .lossy(false)
            .finish(std::io::sink()),
        OutputTarget::Stdout => NonBlockingBuilder::default()
            .buffered_lines_limit(CHANNEL_SIZE)
            .lossy(false)
            .finish(std::io::stdout()),
        OutputTarget::File => {
            let file = std::fs::File::create("logs/tracing_async_conc.log").unwrap();
            NonBlockingBuilder::default()
                .buffered_lines_limit(CHANNEL_SIZE)
                .lossy(false)
                .finish(file)
        }
    };
    let subscriber = fmt()
        .json()
        .flatten_event(true)
        .with_writer(writer)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting tracing default failed");
    // _guard outlives run_concurrent — background thread stays alive until after threads join
    let result = run_concurrent(|| || event!(Level::INFO, "{} {}", MESSAGE, "tracing_async"));
    drop(_guard);
    result
}

fn bench_winston_concurrent(target: OutputTarget) -> (f64, u64) {
    let builder = winston::Logger::builder()
        .channel_capacity(200_000)
        .backpressure_strategy(winston::BackpressureStrategy::Block)
        .format(
            winston::format::timestamp().chain(winston::format::printf(|info| {
                format!(
                    r#"{{"timestamp":"{}","level":"{}","target":"logmark","message":"{}"}}"#,
                    info.meta
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    info.level.to_ascii_uppercase(),
                    info.message
                )
            })),
        );
    let logger = match target {
        OutputTarget::Sink => builder
            .transport(winston::transports::WriterTransport::new(std::io::sink()))
            .build(),
        OutputTarget::Stdout => builder
            .transport(winston::transports::WriterTransport::new(BufWriter::new(
                std::io::stdout(),
            )))
            .build(),
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/winston_conc.log").unwrap();
            builder
                .transport(winston::transports::WriterTransport::new(BufWriter::new(
                    log_file,
                )))
                .build()
        }
    };
    let logger = Arc::new(logger);
    run_concurrent(move || {
        let logger = Arc::clone(&logger);
        move || winston::log!(*logger, info, format!("{} {}", MESSAGE, "winston"))
    })
}

fn bench_slog_async_saturate(target: OutputTarget) -> (f64, u64) {
    macro_rules! async_drain {
        ($w:expr) => {
            slog_async::Async::new(PrebufDrain::new($w).fuse())
                .chan_size(SATURATION_CHANNEL_SIZE)
                .overflow_strategy(slog_async::OverflowStrategy::Block)
                .build()
                .fuse()
        };
    }

    let drain = match target {
        OutputTarget::Sink => async_drain!(std::io::sink()),
        OutputTarget::Stdout => async_drain!(std::io::stdout()),
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/slog_async_sat.log").unwrap();
            async_drain!(log_file)
        }
    };
    let root = Logger::root(drain, o!());
    run_concurrent(move || {
        let root = root.clone();
        move || slog::info!(root, "{} {}", MESSAGE, "slog_async")
    })
}

fn bench_tracing_async_saturate(target: OutputTarget) -> (f64, u64) {
    use tracing_appender::non_blocking::NonBlockingBuilder;
    use tracing_subscriber::fmt;
    let (writer, _guard) = match target {
        OutputTarget::Sink => NonBlockingBuilder::default()
            .buffered_lines_limit(SATURATION_CHANNEL_SIZE)
            .lossy(false)
            .finish(std::io::sink()),
        OutputTarget::Stdout => NonBlockingBuilder::default()
            .buffered_lines_limit(SATURATION_CHANNEL_SIZE)
            .lossy(false)
            .finish(std::io::stdout()),
        OutputTarget::File => {
            let file = std::fs::File::create("logs/tracing_async_sat.log").unwrap();
            NonBlockingBuilder::default()
                .buffered_lines_limit(SATURATION_CHANNEL_SIZE)
                .lossy(false)
                .finish(file)
        }
    };
    let subscriber = fmt()
        .json()
        .flatten_event(true)
        .with_writer(writer)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting tracing default failed");
    // _guard must outlive run_concurrent so the background thread stays alive until threads join
    let result = run_concurrent(|| || event!(Level::INFO, "{} {}", MESSAGE, "tracing_async"));
    drop(_guard);
    result
}

fn bench_winston_saturate(target: OutputTarget) -> (f64, u64) {
    let builder = winston::Logger::builder()
        .channel_capacity(SATURATION_CHANNEL_SIZE)
        .backpressure_strategy(winston::BackpressureStrategy::Block)
        .format(
            winston::format::timestamp().chain(winston::format::printf(|info| {
                format!(
                    r#"{{"timestamp":"{}","level":"{}","target":"logmark","message":"{}"}}"#,
                    info.meta
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    info.level.to_ascii_uppercase(),
                    info.message
                )
            })),
        );
    let logger = match target {
        OutputTarget::Sink => builder
            .transport(winston::transports::WriterTransport::new(std::io::sink()))
            .build(),
        OutputTarget::Stdout => builder
            .transport(winston::transports::WriterTransport::new(BufWriter::new(
                std::io::stdout(),
            )))
            .build(),
        OutputTarget::File => {
            let log_file = std::fs::File::create("logs/winston_sat.log").unwrap();
            builder
                .transport(winston::transports::WriterTransport::new(BufWriter::new(
                    log_file,
                )))
                .build()
        }
    };
    let logger = Arc::new(logger);
    run_concurrent(move || {
        let logger = Arc::clone(&logger);
        move || winston::log!(*logger, info, format!("{} {}", MESSAGE, "winston"))
    })
}

fn run_concurrent_benchmark(benchmark_name: &str, target_name: &str) {
    let target = match target_name {
        "sink" => OutputTarget::Sink,
        "stdout" => OutputTarget::Stdout,
        "file" => OutputTarget::File,
        _ => panic!("Unknown target: {}", target_name),
    };
    let (ops, p99) = match benchmark_name {
        "env_logger" => bench_env_logger_concurrent(target),
        "fern" => bench_fern_concurrent(target),
        "slog" => bench_slog_concurrent(target),
        "slog_async" => bench_slog_async_concurrent(target),
        "tracing" => bench_tracing_concurrent(target),
        "tracing_async" => bench_tracing_async_concurrent(target),
        "winston" => bench_winston_concurrent(target),
        _ => panic!("Unknown benchmark: {}", benchmark_name),
    };
    println!(
        "LOGMARK_CONC: {} {} {:.4} {}",
        benchmark_name, target_name, ops, p99
    );
}

fn run_saturation_benchmark(benchmark_name: &str, target_name: &str) {
    let target = match target_name {
        "sink" => OutputTarget::Sink,
        "stdout" => OutputTarget::Stdout,
        "file" => OutputTarget::File,
        _ => panic!("Unknown target: {}", target_name),
    };
    let (ops, p99) = match benchmark_name {
        "slog_async" => bench_slog_async_saturate(target),
        "tracing_async" => bench_tracing_async_saturate(target),
        "winston" => bench_winston_saturate(target),
        _ => panic!("Saturation benchmark not supported for: {}", benchmark_name),
    };
    println!(
        "LOGMARK_SAT: {} {} {:.4} {}",
        benchmark_name, target_name, ops, p99
    );
}

fn run_individual_benchmark(benchmark_name: &str, target_name: &str) -> BenchmarkResult {
    let target = match target_name {
        "sink" => OutputTarget::Sink,
        "stdout" => OutputTarget::Stdout,
        "file" => OutputTarget::File,
        _ => panic!("Unknown target: {}", target_name),
    };

    let result = match benchmark_name {
        "env_logger" => bench_env_logger(target),
        "fern" => bench_fern(target),
        "slog" => bench_slog(target),
        "slog_async" => bench_slog_async(target),
        "tracing" => bench_tracing(target),
        "tracing_async" => bench_tracing_async(target),
        "winston" => bench_winston(target),
        _ => panic!("Unknown benchmark: {}", benchmark_name),
    };

    println!(
        "LOGMARK: {} {} {:.4} {:.4} {:.4} {:.6} {:.6} {} {} {} {}",
        result.name,
        result.target.as_str(),
        result.elapsed,
        result.ops,
        result.memory_usage,
        result.startup_secs,
        result.drain_secs,
        result.p50_ns,
        result.p99_ns,
        result.p999_ns,
        result.max_ns,
    );

    result
}

fn run_benchmarks_in_processes(
    benchmarks: &[&str],
    targets: &[OutputTarget],
    runs: usize,
) -> (
    HashMap<String, BenchmarkStats>,
    HashMap<String, ConcurrentStats>,
    HashMap<String, ConcurrentStats>,
) {
    let mut results: HashMap<String, BenchmarkStats> = HashMap::new();
    let mut conc_results: HashMap<String, ConcurrentStats> = HashMap::new();
    let mut sat_results: HashMap<String, ConcurrentStats> = HashMap::new();
    let mut rng = thread_rng();

    let _ = fs::create_dir_all("benchmark_results");
    let _ = fs::create_dir_all("logs");

    let mut all_benchmarks: Vec<(&str, OutputTarget)> = benchmarks
        .iter()
        .flat_map(|&bench| targets.iter().map(move |&target| (bench, target)))
        .collect();

    for warmup in 1..=NUM_WARMUP_RUNS {
        println!(
            "\n-- warmup {} of {}  [{}] --",
            warmup,
            NUM_WARMUP_RUNS,
            Local::now().format("%H:%M:%S")
        );
        all_benchmarks.shuffle(&mut rng);
        for &(bench, target) in &all_benchmarks {
            println!("  {} ({})", bench, target.as_str());
            let start = Instant::now();
            let _ = Command::new(env::current_exe().unwrap())
                .arg("--benchmark")
                .arg(bench)
                .arg(target.as_str())
                .output();
            println!("    done  {:.4}s", start.elapsed().as_secs_f64());
        }
    }

    for run in 1..=runs {
        println!(
            "\n-- run {} of {}  [{}] --",
            run,
            runs,
            Local::now().format("%H:%M:%S")
        );
        all_benchmarks.shuffle(&mut rng);

        for &(bench, target) in &all_benchmarks {
            println!("  {} ({})", bench, target.as_str());

            let output = Command::new(env::current_exe().unwrap())
                .arg("--benchmark")
                .arg(bench)
                .arg(target.as_str())
                .output()
                .expect("Failed to run benchmark");

            if output.status.success() {
                let output_str = String::from_utf8(output.stdout).unwrap();

                if let Some(line) = output_str.lines().find(|l| l.starts_with("LOGMARK: ")) {
                    let result_parts: Vec<&str> =
                        line["LOGMARK: ".len()..].split_whitespace().collect();
                    if result_parts.len() >= 11 {
                        let name = format!("{}_{}", result_parts[0], result_parts[1]);
                        let elapsed: f64 = result_parts[2].parse().unwrap();
                        let ops: f64 = result_parts[3].parse().unwrap();
                        let memory: f64 = result_parts[4].parse().unwrap();
                        let startup: f64 = result_parts[5].parse().unwrap();
                        let drain: f64 = result_parts[6].parse().unwrap();
                        let p50: u64 = result_parts[7].parse().unwrap();
                        let p99: u64 = result_parts[8].parse().unwrap();
                        let p999: u64 = result_parts[9].parse().unwrap();
                        let max: u64 = result_parts[10].parse().unwrap();

                        let stats = results.entry(name.clone()).or_default();
                        stats.elapsed_times.push(elapsed);
                        stats.ops_rates.push(ops);
                        stats.memory_usages.push(memory);
                        stats.startup_times.push(startup);
                        stats.drain_times.push(drain);
                        stats.p50_ns.push(p50);
                        stats.p99_ns.push(p99);
                        stats.p999_ns.push(p999);
                        stats.max_ns.push(max);

                        println!(
                            "    [{}] done  {:.4}s  {}  {}",
                            Local::now().format("%H:%M:%S"),
                            elapsed,
                            fmt_ops(ops),
                            fmt_mem(memory)
                        );
                    }
                }
            } else {
                eprintln!(
                    "Benchmark {} ({}) failed:\n{}",
                    bench,
                    target.as_str(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // Concurrent pass — no warmup, same NUM_RUNS.
    for run in 1..=runs {
        println!(
            "\n-- concurrent run {} of {}  [{}] --",
            run,
            runs,
            Local::now().format("%H:%M:%S")
        );
        all_benchmarks.shuffle(&mut rng);

        for &(bench, target) in &all_benchmarks {
            println!("  {} ({}) ×{}", bench, target.as_str(), CONCURRENT_THREADS);

            let output = Command::new(env::current_exe().unwrap())
                .arg("--concurrent")
                .arg(bench)
                .arg(target.as_str())
                .output()
                .expect("Failed to run concurrent benchmark");

            if output.status.success() {
                let output_str = String::from_utf8(output.stdout).unwrap();

                if let Some(line) = output_str.lines().find(|l| l.starts_with("LOGMARK_CONC: ")) {
                    let parts: Vec<&str> =
                        line["LOGMARK_CONC: ".len()..].split_whitespace().collect();
                    if parts.len() >= 4 {
                        let name = format!("{}_{}", parts[0], parts[1]);
                        let ops: f64 = parts[2].parse().unwrap();
                        let p99: u64 = parts[3].parse().unwrap();

                        let stats = conc_results.entry(name).or_default();
                        stats.ops_rates.push(ops);
                        stats.p99_ns.push(p99);

                        println!(
                            "    [{}] done  {}  p99={}",
                            Local::now().format("%H:%M:%S"),
                            fmt_ops(ops),
                            fmt_latency(p99),
                        );
                    }
                }
            } else {
                eprintln!(
                    "Concurrent benchmark {} ({}) failed:\n{}",
                    bench,
                    target.as_str(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // Saturation pass — async loggers only, intentionally undersized channel.
    let async_benchmarks = ["slog_async", "tracing_async", "winston"];
    let mut sat_pairs: Vec<(&str, OutputTarget)> = async_benchmarks
        .iter()
        .flat_map(|&bench| targets.iter().map(move |&target| (bench, target)))
        .collect();

    for run in 1..=runs {
        println!(
            "\n-- saturation run {} of {}  [{}] --",
            run,
            runs,
            Local::now().format("%H:%M:%S")
        );
        sat_pairs.shuffle(&mut rng);

        for &(bench, target) in &sat_pairs {
            println!(
                "  {} ({}) ×{} [sat]",
                bench,
                target.as_str(),
                CONCURRENT_THREADS
            );

            let output = Command::new(env::current_exe().unwrap())
                .arg("--saturate")
                .arg(bench)
                .arg(target.as_str())
                .output()
                .expect("Failed to run saturation benchmark");

            if output.status.success() {
                let output_str = String::from_utf8(output.stdout).unwrap();

                if let Some(line) = output_str.lines().find(|l| l.starts_with("LOGMARK_SAT: ")) {
                    let parts: Vec<&str> =
                        line["LOGMARK_SAT: ".len()..].split_whitespace().collect();
                    if parts.len() >= 4 {
                        let name = format!("{}_{}", parts[0], parts[1]);
                        let ops: f64 = parts[2].parse().unwrap();
                        let p99: u64 = parts[3].parse().unwrap();

                        let stats = sat_results.entry(name).or_default();
                        stats.ops_rates.push(ops);
                        stats.p99_ns.push(p99);

                        println!(
                            "    [{}] done  {}  p99={}",
                            Local::now().format("%H:%M:%S"),
                            fmt_ops(ops),
                            fmt_latency(p99),
                        );
                    }
                }
            } else {
                eprintln!(
                    "Saturation benchmark {} ({}) failed:\n{}",
                    bench,
                    target.as_str(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    (results, conc_results, sat_results)
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if sorted.len() % 2 == 0 {
        let mid = sorted.len() / 2;
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[sorted.len() / 2]
    }
}

fn std_dev(values: &[f64]) -> f64 {
    if values.len() <= 1 {
        return 0.0;
    }
    let m = mean(values);
    let variance =
        values.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (values.len() as f64 - 1.0);
    variance.sqrt()
}

fn fmt_ops(ops: f64) -> String {
    if ops >= 1_000_000.0 {
        format!("{:.2}M", ops / 1_000_000.0)
    } else if ops >= 1_000.0 {
        format!("{:.1}K", ops / 1_000.0)
    } else {
        format!("{:.0}", ops)
    }
}

fn median_u64(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut s = values.to_vec();
    s.sort_unstable();
    s[s.len() / 2]
}

fn fmt_latency(ns: u64) -> String {
    if ns < 1_000 {
        format!("{}ns", ns)
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else {
        format!("{:.1}ms", ns as f64 / 1_000_000.0)
    }
}

fn fmt_mem(mb: f64) -> String {
    if mb >= 1.0 {
        format!("{:.2}MB", mb)
    } else if mb >= 0.001 {
        format!("{:.1}KB", mb * 1024.0)
    } else {
        let bytes = (mb * 1024.0 * 1024.0).round() as u64;
        format!("{}B", bytes)
    }
}

fn make_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut builder = Builder::default();
    builder.push_record(headers.iter().copied());
    for row in rows {
        builder.push_record(row.iter().map(String::as_str));
    }
    let mut table = builder.build();
    table.with(Style::modern());
    table.to_string()
}

// Coefficient of variation: stddev / mean. Used to flag unreliable runs.
fn cov(values: &[f64]) -> f64 {
    let m = mean(values);
    if m == 0.0 {
        return 0.0;
    }
    std_dev(values) / m
}

fn print_stats_report(
    stats: &HashMap<String, BenchmarkStats>,
    conc_stats: &HashMap<String, ConcurrentStats>,
    sat_stats: &HashMap<String, ConcurrentStats>,
    run_timestamp: &str,
    logger_count: usize,
) {
    let sep = "─".repeat(65);
    println!("\n{sep}");
    println!("  logmark  {run_timestamp}");
    println!("  {logger_count} loggers × 3 targets × {NUM_RUNS} runs × {ITERATIONS} iterations");
    println!("{sep}");

    // Detailed per-run file (plain text, all entries sorted alphabetically).
    let mut detail = fs::File::create("benchmark_results/detailed_stats.txt").unwrap();
    writeln!(detail, "logmark  {run_timestamp}").unwrap();
    writeln!(
        detail,
        "{logger_count} loggers × 3 targets × {NUM_RUNS} runs × {ITERATIONS} iterations\n"
    )
    .unwrap();
    let mut all_names: Vec<&String> = stats.keys().collect();
    all_names.sort();
    for name in &all_names {
        let stat = &stats[*name];
        let c = cov(&stat.elapsed_times);
        let flag = if c > 0.25 { " [!]" } else { "" };
        write!(detail, "{:<26}", name).unwrap();
        for (i, t) in stat.elapsed_times.iter().enumerate() {
            write!(detail, "  run{}={:.4}s", i + 1, t).unwrap();
        }
        let drain_part = if stat.drain_times.iter().all(|&d| d == 0.0) {
            String::new()
        } else {
            format!("  drain={:.4}s", median(&stat.drain_times))
        };
        writeln!(
            detail,
            "  median={:.4}s  ±{:.0}%{}  heap=+{}  init={:.4}s{}",
            median(&stat.elapsed_times),
            c * 100.0,
            flag,
            fmt_mem(median(&stat.memory_usages)),
            median(&stat.startup_times),
            drain_part,
        )
        .unwrap();
    }

    // Throughput tables — one per target, fastest first.
    for target in &["sink", "stdout", "file"] {
        let mut group: Vec<(&str, &BenchmarkStats)> = stats
            .iter()
            .filter_map(|(name, stat)| {
                name.rsplit_once('_')
                    .filter(|(_, t)| t == target)
                    .map(|(logger, _)| (logger, stat))
            })
            .collect();

        if group.is_empty() {
            continue;
        }

        group.sort_by(|(_, a), (_, b)| {
            median(&b.ops_rates)
                .partial_cmp(&median(&a.ops_rates))
                .unwrap()
        });

        let best_ops = median(&group[0].1.ops_rates);
        let mut any_flagged = false;
        let mut rows: Vec<Vec<String>> = Vec::new();

        for (rank, (logger, stat)) in group.iter().enumerate() {
            let med_ops = median(&stat.ops_rates);
            let med_time = median(&stat.elapsed_times);
            let vs = best_ops / med_ops;
            let c = cov(&stat.elapsed_times);
            let flag = if c > 0.25 {
                any_flagged = true;
                " [!]"
            } else {
                ""
            };
            let drain_str = if stat.drain_times.iter().all(|&d| d == 0.0) {
                "-".to_string()
            } else {
                format!("{:.4}s", median(&stat.drain_times))
            };
            rows.push(vec![
                format!("{}", rank + 1),
                logger.to_string(),
                fmt_ops(med_ops),
                format!("{:.4}s", med_time),
                format!("{:.1}×", vs),
                format!("±{:.0}%{}", c * 100.0, flag),
                format!("{:.4}s", median(&stat.startup_times)),
                drain_str,
            ]);
        }

        let headers = [
            "#",
            "logger",
            "median ops/s",
            "median time",
            "vs best",
            "var",
            "init",
            "drain",
        ];
        let table_str = make_table(&headers, &rows);

        println!("\n  ── {} ", target.to_uppercase());
        println!("{}", table_str);
        if any_flagged {
            println!("  [!] CoV > 25% — high variance; median is more reliable than mean");
        }

        let path = format!("benchmark_results/{target}_summary.txt");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "logmark  {target} target  {run_timestamp}").unwrap();
        writeln!(
            f,
            "{logger_count} loggers × {NUM_RUNS} runs × {ITERATIONS} iterations\n"
        )
        .unwrap();
        writeln!(f, "{}", table_str).unwrap();
    }

    // Latency tables — one per target, sorted by P99 ascending.
    println!("\n{sep}");
    println!("  latency  ({LATENCY_ITERATIONS} samples per run, nanosecond resolution)");
    println!("  async loggers show enqueue latency; sync loggers show write latency");
    println!("{sep}");

    for target in &["sink", "stdout", "file"] {
        let mut group: Vec<(&str, &BenchmarkStats)> = stats
            .iter()
            .filter_map(|(name, stat)| {
                name.rsplit_once('_')
                    .filter(|(_, t)| t == target)
                    .map(|(logger, _)| (logger, stat))
            })
            .collect();

        if group.is_empty() {
            continue;
        }

        group.sort_by_key(|(_, s)| median_u64(&s.p99_ns));

        let headers = ["#", "logger", "P50", "P99", "P99.9", "max"];
        let rows: Vec<Vec<String>> = group
            .iter()
            .enumerate()
            .map(|(rank, (logger, stat))| {
                vec![
                    format!("{}", rank + 1),
                    logger.to_string(),
                    fmt_latency(median_u64(&stat.p50_ns)),
                    fmt_latency(median_u64(&stat.p99_ns)),
                    fmt_latency(median_u64(&stat.p999_ns)),
                    fmt_latency(median_u64(&stat.max_ns)),
                ]
            })
            .collect();

        println!("\n  ── {} ", target.to_uppercase());
        println!("{}", make_table(&headers, &rows));
    }

    // Concurrent throughput tables — one per target.
    println!("\n{sep}");
    println!(
        "  concurrency  ({} threads × {} iters/thread)",
        CONCURRENT_THREADS, CONC_ITERS_PER_THREAD
    );
    println!("  scale = conc ops/s ÷ seq ops/s  (ideal ≈ {CONCURRENT_THREADS}×)");
    println!("{sep}");

    for target in &["sink", "stdout", "file"] {
        let mut group: Vec<(&str, f64, f64, u64)> = stats
            .iter()
            .filter_map(|(name, seq_stat)| {
                name.rsplit_once('_')
                    .filter(|(_, t)| t == target)
                    .and_then(|(logger, _)| {
                        conc_stats.get(name.as_str()).map(|cs| {
                            (
                                logger,
                                median(&seq_stat.ops_rates),
                                median(&cs.ops_rates),
                                median_u64(&cs.p99_ns),
                            )
                        })
                    })
            })
            .collect();

        if group.is_empty() {
            continue;
        }

        group.sort_by(|(_, _, a_ops, _), (_, _, b_ops, _)| b_ops.partial_cmp(a_ops).unwrap());

        let headers = [
            "#",
            "logger",
            "conc ops/s",
            "seq ops/s",
            "scale",
            "conc P99",
        ];
        let rows: Vec<Vec<String>> = group
            .iter()
            .enumerate()
            .map(|(rank, (logger, seq_ops, conc_ops, p99))| {
                vec![
                    format!("{}", rank + 1),
                    logger.to_string(),
                    fmt_ops(*conc_ops),
                    fmt_ops(*seq_ops),
                    format!("{:.1}×", conc_ops / seq_ops),
                    fmt_latency(*p99),
                ]
            })
            .collect();

        println!("\n  ── {} ", target.to_uppercase());
        println!("{}", make_table(&headers, &rows));
    }

    // Saturation tables — one per target, async loggers only.
    println!("\n{sep}");
    println!(
        "  saturation  (channel={SATURATION_CHANNEL_SIZE}, {} threads × {} iters = {} total)",
        CONCURRENT_THREADS,
        CONC_ITERS_PER_THREAD,
        CONCURRENT_THREADS * CONC_ITERS_PER_THREAD,
    );
    println!("  tput Δ = (sat − conc) / conc  |  P99 spike = sat P99 / conc P99");
    println!("{sep}");

    for target in &["sink", "stdout", "file"] {
        let mut group: Vec<(&str, f64, f64, u64, u64)> = conc_stats
            .iter()
            .filter_map(|(name, cs)| {
                name.rsplit_once('_')
                    .filter(|(_, t)| t == target)
                    .and_then(|(logger, _)| {
                        sat_stats.get(name).map(|ss| {
                            (
                                logger,
                                median(&cs.ops_rates),
                                median(&ss.ops_rates),
                                median_u64(&cs.p99_ns),
                                median_u64(&ss.p99_ns),
                            )
                        })
                    })
            })
            .collect();

        if group.is_empty() {
            continue;
        }

        group.sort_by(|(_, _, a_sat, _, _), (_, _, b_sat, _, _)| b_sat.partial_cmp(a_sat).unwrap());

        let headers = [
            "#",
            "logger",
            "sat ops/s",
            "conc ops/s",
            "tput Δ",
            "sat P99",
            "conc P99",
            "P99 spike",
        ];
        let rows: Vec<Vec<String>> = group
            .iter()
            .enumerate()
            .map(|(rank, (logger, conc_ops, sat_ops, conc_p99, sat_p99))| {
                let tput_delta = (sat_ops - conc_ops) / conc_ops * 100.0;
                let p99_spike = if *conc_p99 > 0 {
                    *sat_p99 as f64 / *conc_p99 as f64
                } else {
                    0.0
                };
                vec![
                    format!("{}", rank + 1),
                    logger.to_string(),
                    fmt_ops(*sat_ops),
                    fmt_ops(*conc_ops),
                    format!("{:+.1}%", tput_delta),
                    fmt_latency(*sat_p99),
                    fmt_latency(*conc_p99),
                    format!("{:.1}×", p99_spike),
                ]
            })
            .collect();

        println!("\n  ── {} ", target.to_uppercase());
        println!("{}", make_table(&headers, &rows));
    }

    println!("\nResults written to benchmark_results/");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() > 3 && args[1] == "--benchmark" {
        let _ = run_individual_benchmark(&args[2], &args[3]);
        exit(0);
    }
    if args.len() > 3 && args[1] == "--concurrent" {
        run_concurrent_benchmark(&args[2], &args[3]);
        exit(0);
    }
    if args.len() > 3 && args[1] == "--saturate" {
        run_saturation_benchmark(&args[2], &args[3]);
        exit(0);
    }

    let benchmarks = vec![
        // Sync — run on the caller's thread
        "env_logger",
        "fern",
        "slog",
        "tracing",
        // Async — internal worker thread
        "slog_async",
        "tracing_async",
        "winston",
    ];

    let targets = OutputTarget::all();
    let run_timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    println!(
        "logmark  {}  {} loggers × {} targets × {} warmup + {} runs",
        run_timestamp,
        benchmarks.len(),
        targets.len(),
        NUM_WARMUP_RUNS,
        NUM_RUNS
    );

    let (stats, conc_stats, sat_stats) =
        run_benchmarks_in_processes(&benchmarks, &targets, NUM_RUNS);
    print_stats_report(
        &stats,
        &conc_stats,
        &sat_stats,
        &run_timestamp,
        benchmarks.len(),
    );
}
