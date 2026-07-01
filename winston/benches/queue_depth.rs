//! Sustained-drain throughput vs queue depth.
//!
//! Answers the question ADR 0006 left open: does the WritableStream high-water
//! mark's throughput benefit actually plateau at some depth (which would justify
//! a `WS_HWM = min(capacity, X)` cap for very large capacities), or does it keep
//! climbing?
//!
//! `logger_benchmark`'s `noop` group can't answer it: it logs a fixed 1000-entry
//! burst, so once the WS HWM exceeds the burst the pump never parks and the curve
//! flattens at the burst size — an artifact, not a real plateau. Here each
//! measurement logs `BATCH` entries with `BATCH` >> any swept depth, so the
//! queues stay full and the pump↔controller handoff is exercised continuously.
//!
//! Setup chosen so the *consumer chain* (pump → WS controller → sink) is the
//! bottleneck — the only regime where the WS HWM affects throughput:
//! - passthrough format → the producer is cheap, so it outruns the consumer;
//! - counting (near-noop) sink → the bottleneck is the queue machinery, not I/O;
//! - `Block` policy → no drops, and the producer is throttled to the drain rate
//!   once the queues fill (which warmup ensures), so simply timing `BATCH`
//!   `log()` calls measures the drain rate. No per-batch flush: a flush tail
//!   would scale with the WS depth and confound the depth comparison.
//!
//! This complements `logger_benchmark`'s burst+flush `noop` group, which favors
//! a *deep* WS (a short burst drained under a waiting flush parks less with more
//! room). Sustained drain favors a *shallow* WS (cache-warm pump→controller
//! handoff). The WS HWM is a genuine workload-dependent trade, not a single
//! optimum — this bench measures the sustained side of it.
//!
//! By default sweeps `capacity`, which sets the mailbox *and* the WS HWM. To
//! isolate the WS HWM from the mailbox, build with `--features internal-bench`
//! and set `WINSTON_WS_HWM` to pin the WS depth while `capacity` (the mailbox)
//! stays fixed:
//!
//! ```text
//! for h in 256 1024 4096 16384 65536; do
//!   WINSTON_WS_HWM=$h cargo bench --features internal-bench --bench queue_depth
//! done
//! ```

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use logform::{FinalizeExt, FormattedEntry, LogInfo};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use whatwg_streams::StreamResult;
use winston::{Logger, LoggerTransport, OverflowPolicy, Transport};

/// Entries per measured batch. Must stay well above any swept queue depth so the
/// queues run at steady state rather than absorbing the whole batch.
const BATCH: u64 = 500_000;

/// Near-noop sink: counts writes so the work isn't elided, but does no I/O — the
/// bottleneck stays the queue machinery.
struct CountingSink {
    count: Arc<AtomicU64>,
}

impl Transport for CountingSink {
    async fn log(&mut self, _entry: FormattedEntry) -> StreamResult<()> {
        self.count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn bench_sustained_drain(c: &mut Criterion) {
    let mut group = c.benchmark_group("sustained_drain");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(15));
    group.throughput(Throughput::Elements(BATCH));

    for cap in [256usize, 1024, 4096, 16384, 65536] {
        group.bench_with_input(BenchmarkId::from_parameter(cap), &cap, |b, &cap| {
            let transport = LoggerTransport::new(CountingSink {
                count: Arc::new(AtomicU64::new(0)),
            })
            .with_overflow_policy(OverflowPolicy::Block)
            .with_queue_capacity(cap);
            let logger = Logger::builder()
                .format(logform::passthrough().into_pipeline())
                .transport(transport)
                .build();

            // No flush inside the timed loop: under Block the producer is
            // already throttled to the drain rate once the queues fill (which
            // warmup ensures), so logging BATCH entries measures the drain rate
            // directly — without a flush tail whose size would scale with the WS
            // depth and confound the comparison. The residual backlog drains in
            // the background and on logger drop.
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    for _ in 0..BATCH {
                        logger.log(LogInfo::new("info", "sustained drain benchmark message"));
                    }
                }
                start.elapsed()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sustained_drain);
criterion_main!(benches);
