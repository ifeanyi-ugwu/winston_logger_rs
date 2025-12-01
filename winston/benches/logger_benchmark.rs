use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use logform::LogInfo;
use std::sync::Arc;
use winston::Logger;

fn benchmark_logging(c: &mut Criterion) {
    // 1. Measure just the logger overhead (no real I/O)
    let mut group = c.benchmark_group("logger_overhead");

    // Mock transport that does nothing (measures pure logger speed)
    #[derive(Clone)]
    struct NoOpTransport;
    impl winston_transport::Transport<LogInfo> for NoOpTransport {
        fn log(&self, _info: LogInfo) {}
        fn flush(&self) -> Result<(), String> {
            Ok(())
        }
        fn query(&self, _: &winston_transport::LogQuery) -> Result<Vec<LogInfo>, String> {
            Ok(vec![])
        }
    }

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
                                for i in 0..(iters / num_threads as u64) {
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
