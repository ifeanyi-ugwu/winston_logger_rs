use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use logform::{FinalizeExt, FormattedEntry, LogInfo};
use std::sync::Arc;
use whatwg_streams::{StreamResult, WritableSink, WritableStreamDefaultController};
use winston::Logger;
use winston_transport::Transport;

fn benchmark_logging(c: &mut Criterion) {
    // 1. Measure just the logger overhead (no real I/O)
    let mut group = c.benchmark_group("logger_overhead");

    // Sink that discards every entry (measures pure logger speed).
    #[derive(Clone)]
    struct NoOpTransport;
    impl WritableSink<FormattedEntry> for NoOpTransport {
        async fn write(
            &mut self,
            _entry: FormattedEntry,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            Ok(())
        }
    }
    impl Transport for NoOpTransport {}

    group.throughput(Throughput::Elements(1000));
    group.bench_function("noop_transport", |b| {
        let logger = Logger::builder().transport(NoOpTransport).build();

        b.iter(|| {
            for _ in 0..1000 {
                logger.log(black_box(LogInfo::new("info", "benchmark message")));
            }
            logger.flush().unwrap(); // IMPORTANT: measure full pipeline
        });
    });

    group.finish();

    // 1b. Fan-out render scaling: K transports all inheriting the default
    // json() global format, so each renders the entry per slot. Shows whether
    // caller throughput degrades with transport count — i.e. whether the
    // per-inherit-slot render is a real cost that Layer 2's render-dedup
    // (render once, share across inherit-slots) would remove.
    let mut group = c.benchmark_group("fanout_render");
    for k in [1usize, 2, 4, 8] {
        group.throughput(Throughput::Elements(1000));
        group.bench_with_input(BenchmarkId::from_parameter(k), &k, |b, &k| {
            let mut builder = Logger::builder();
            for _ in 0..k {
                builder = builder.transport(NoOpTransport);
            }
            let logger = builder.build();
            b.iter(|| {
                for _ in 0..1000 {
                    logger.log(black_box(LogInfo::new("info", "benchmark message")));
                }
                logger.flush().unwrap();
            });
        });
    }
    group.finish();

    // 1c. Same fan-out, but passthrough (no render) — isolates the per-slot
    // render (json above) from the K-way fan-out coordination overhead. If this
    // scales the same as `fanout_render`, render is not the cost and Layer 2's
    // render-dedup would not help.
    let mut group = c.benchmark_group("fanout_passthrough");
    for k in [1usize, 2, 4, 8] {
        group.throughput(Throughput::Elements(1000));
        group.bench_with_input(BenchmarkId::from_parameter(k), &k, |b, &k| {
            let mut builder =
                Logger::builder().format(logform::passthrough().into_pipeline());
            for _ in 0..k {
                builder = builder.transport(NoOpTransport);
            }
            let logger = builder.build();
            b.iter(|| {
                for _ in 0..1000 {
                    logger.log(black_box(LogInfo::new("info", "benchmark message")));
                }
                logger.flush().unwrap();
            });
        });
    }
    group.finish();

    // 2. Multi-threaded contention test
    let mut group = c.benchmark_group("multi_threaded");

    for num_threads in [1, 2, 4, 8] {
        group.throughput(Throughput::Elements(1000 * num_threads));
        group.bench_with_input(
            BenchmarkId::from_parameter(num_threads),
            &num_threads,
            |b, &num_threads| {
                b.iter_custom(|iters| {
                    let logger = Arc::new(Logger::builder().transport(NoOpTransport).build());

                    let start = std::time::Instant::now();

                    let handles: Vec<_> = (0..num_threads)
                        .map(|_| {
                            let l = Arc::clone(&logger);
                            std::thread::spawn(move || {
                                for i in 0..(iters / num_threads) {
                                    l.log(black_box(LogInfo::new(
                                        "info",
                                        format!("message {}", i),
                                    )));
                                }
                            })
                        })
                        .collect();

                    for h in handles {
                        h.join().unwrap();
                    }

                    logger.flush().unwrap();
                    start.elapsed()
                });
            },
        );
    }

    group.finish();

    // 3. File I/O (realistic workload)
    let mut group = c.benchmark_group("file_io");
    group.sample_size(10); // Fewer samples since file I/O is slow
    group.throughput(Throughput::Elements(1000));

    group.bench_function("file_transport", |b| {
        b.iter(|| {
            let filename = format!(
                "bench_{}.log",
                std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );

            let file_transport = winston::transports::File::builder()
                .filename(&filename)
                .build();

            let logger = Logger::builder().transport(file_transport).build();

            for _ in 0..1000 {
                logger.log(black_box(LogInfo::new("info", "benchmark message")));
            }
            logger.flush().unwrap();

            std::fs::remove_file(&filename).ok();
        });
    });

    group.finish();

    // 4. Varying message sizes
    let mut group = c.benchmark_group("message_size");

    for size in [10, 100, 1000, 10000] {
        let message = "x".repeat(size);
        group.throughput(Throughput::Bytes((size * 1000) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &message, |b, msg| {
            let logger = Logger::builder().transport(NoOpTransport).build();

            b.iter(|| {
                for _ in 0..1000 {
                    logger.log(black_box(LogInfo::new("info", msg.clone())));
                }
                logger.flush().unwrap();
            });
        });
    }

    group.finish();
}

criterion_group!(benches, benchmark_logging);
criterion_main!(benches);
