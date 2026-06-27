# ADR 0007 — Shallow WritableStream high-water mark

**Status:** Accepted

Supersedes the "Single buffer / shallow WS high-water mark" section of
[ADR 0006](0006-lock-free-routing.md), which rejected this change on a
measurement later found to be an artifact. The full derivation is in
[docs/queue-depth-investigation.md](../queue-depth-investigation.md).

## Context

Each transport has two bounded queues in series: the **mailbox** (capacity N,
bearing the slot's `OverflowPolicy`) and the **WritableStream's** internal queue
(high-water mark). They serve different roles — the mailbox is the user-facing
policy/burst-absorption buffer; the WS HWM is purely the internal pump↔controller
throttle (the `ready()` gate), carrying no policy. Originally both were sized to
`capacity`, so worst-case in-flight was `2N`.

ADR 0006 left the WS HWM at `capacity` and recorded shrinking it as
measured-and-rejected — but that measurement came from a burst-then-flush
benchmark whose fixed 1000-entry burst makes throughput top out at `HWM ≥ burst`,
manufacturing a false "deeper is better" plateau. A purpose-built **sustained**
benchmark (`winston/benches/queue_depth.rs`), designed against that and two other
artifacts, reversed the finding.

The sustained, artifact-free measurements establish:

- **The WS HWM's effect on throughput is a cache phenomenon, not handoff
  amortization.** In a saturated pipeline parking is hidden (both threads busy); a
  shallow WS hands the controller cache-warm data the pump just wrote, a deep WS
  forces it to read cold, evicted data. So **deeper WS is slower** in sustained
  logging (~12% at HWM 64→1024, ~27% at 64→65536).
- **The optimum is an absolute entry count, not a fraction of capacity.** An M×W
  matrix shows throughput is `f(WS)` alone — flat across the mailbox depth. The
  knee sits at `W_cache ≈ L1_size / sizeof(FormattedEntry)` (~128 on the test
  machine: ~256 B/entry × 128 ≈ 32 KB = L1), with a flat-optimal plateau ~48–128.
- **The mailbox depth is neutral** for throughput, so keeping it at `capacity`
  (required for policy and burst absorption) is free.

## Decision

Set the WS high-water mark to a fixed cache-fit constant, capped below by the
mailbox so a tiny capacity is never out-deepened:

```rust
pub(crate) const DEFAULT_WS_HWM: usize = 64;
fn resolve_ws_hwm(capacity: usize) -> usize { capacity.min(DEFAULT_WS_HWM) }
```

- `64` sits in the flat-optimal plateau, at its shallow end (least memory), with a
  margin below the L1 knee — a larger entry lowers `W_cache`, so 64 stays inside
  the plateau where 128 might spill L1.
- The mailbox stays `= queue_capacity`. `with_queue_capacity(N)` now means exactly
  what it says — the policy/burst-absorption depth — and worst-case in-flight is
  `N + 64`, not `2N`.
- The constant is decoupled from capacity, **not** a percentage of it: a fraction
  would fall below `W_cache` for small capacities and balloon past it for large
  ones, losing throughput at both ends.

## Consequences

- **+~12% sustained throughput** vs `WS = capacity`, because the pump→controller
  handoff stays cache-warm.
- **~half the worst-case in-flight memory** — `capacity + 64` instead of
  `2 × capacity`. Under a deep `capacity` (e.g. 100k) this is the difference
  between ~100k and ~200k resident entries under sustained saturation.
- **−~7–10% burst-then-flush latency.** A workload that logs a burst and
  immediately flushes (tests, shutdown drain) parks more during the flush's drain
  with a shallow WS. This is the deliberate trade; it does not affect steady-state
  logging, which never flushes per burst, and burst *absorption* (caller latency)
  is unchanged — the mailbox still absorbs the burst.
- **Per-entry throughput is now independent of `capacity`** (the matrix result):
  raising capacity buys burst absorption and a higher backpressure threshold, not
  more or less throughput.
- `DEFAULT_WS_HWM` is machine-tuned (`L1 / sizeof(entry)`); 64 is a conservative
  default across typical entry sizes, not a universal constant. The
  `internal-bench` feature's `WINSTON_WS_HWM` override exists to re-derive it.
- An **advanced per-transport escape hatch** is provided —
  `LoggerTransport::with_ws_high_water_mark` — for the cases the default cannot
  serve: a **batching sink** (writes N entries per call, wants a deeper WS so the
  controller pulls a batch) or measured hardware/entry-size tuning. It is *not*
  the recommended path: its docs lead with the counterintuitive direction (deeper
  is usually worse) to deter cargo-cult tuning, since the WS HWM's intuitive
  "bigger = safer" reading is wrong. An explicit value is honored as-is (not
  capped to `capacity`); the default `min(capacity, 64)` remains the answer for
  ~all transports.

## Alternatives considered and rejected

- **WS = capacity** (ADR 0006's choice). Best burst-then-flush latency, but ~12%
  slower sustained throughput and `2N` worst-case memory. Rejected: sustained
  continuous logging is the representative workload; the burst-flush advantage
  leans on a flush that is a benchmark measurement device, not normal usage.
- **WS as a fraction / derivative of capacity** (half/half, `capacity/k`).
  Refuted directly by the M×W matrix: the knee is absolute, so any fraction is
  wrong at every scale except one.
- **`min(capacity, X)` cap on top of WS = capacity.** The inverse framing of this
  ADR, considered when the plateau was still thought to be ~1024. Subsumed: with
  the knee derived at ~128, the "cap" *is* the default, and at a much smaller
  value than 1024.

## References

- `docs/queue-depth-investigation.md` — the full derivation, the three artifacts,
  and all measurements.
- `winston/benches/queue_depth.rs` — the sustained-drain benchmark and the
  `internal-bench` WS-override hook.
- `winston/src/pipeline.rs` — `DEFAULT_WS_HWM`, `resolve_ws_hwm`, `build_slot`.
- ADR 0006 — Lock-free routing (the change this was scoped out of).
- ADR 0002 — Direct-dispatch backpressure; ADR 0004 — two tasks per transport
  (the pump↔controller split whose handoff the WS HWM throttles).
