# ADR 0006 — Lock-free routing for the dispatch hot path

**Status:** Accepted

## Context

`Logger::log` runs on every call, on the caller's thread. Before this change it
reached the slot list through a `parking_lot::RwLock<LoggerState>`:

```text
log(entry)
  → state.read()                 // RwLock read guard
  → LoggerState::snapshot()      // clones, to release the lock before a
                                 // possible Block-policy park:
      slots:        Vec<Arc<TransportSlot>>   // Vec alloc + N Arc clones
      global_format: Option<Arc<FormatPipeline>>   // Arc clone
      global_level:  Option<String>           // String clone ("info", ...)
      levels:        Option<LoggerLevels>     // HashMap clone (~5 entries)
  → dispatch against the snapshot
```

The snapshot exists for a real reason — dispatch must not hold the state lock
across a `Block`-policy `push_blocking` that parks the producer, or admin
operations would stall behind a slow sink (ADR 0002). But it pays for that with
an allocation-heavy clone on **every** `log()`: a `Vec` allocation, N `Arc`
refcount bumps, a `String` clone, and a `HashMap` clone, none of which the
dispatch actually mutates.

That per-call clone is pure overhead on the hottest path in the library.

## Decision

Store the dispatch-time routing in an immutable struct behind `arc_swap::ArcSwap`
and read it lock-free:

```rust
struct Routing {
    slots: Vec<Arc<TransportSlot>>,
    global_format: Option<Arc<FormatPipeline>>,
    global_level: Option<String>,
    levels: Option<LoggerLevels>,
    buffer: Arc<Mutex<VecDeque<Arc<LogInfo>>>>,
    event_senders: EventSenders,
}

// Logger:
routing: ArcSwap<Routing>,
admin_lock: parking_lot::Mutex<()>,
```

- **`log()` does one `routing.load_full()`** — a single atomic `Arc` clone, no
  `Vec`/`String`/`HashMap` copy, no lock — and dispatches against the loaded
  `Arc<Routing>`. A `Block`-policy park holds only that `Arc`, never a lock, so
  it preserves the property the old snapshot was emulating (dispatch never holds
  the routing lock across a park) for free.
- **Admin operations** (`add` / `remove` / `configure` / `close`) serialize on a
  small mutex, build a new `Routing` (cloning the slot list — rare, off the hot
  path), and `store` it. Readers in flight keep their old `Arc<Routing>` until
  they drop it.

This changes only *how the routing state is stored and read*. The direct-dispatch
backpressure model (ADR 0002), the per-slot mailbox + pump, and the
`FormattedEntry` write boundary (ADR 0005) are all untouched. Formatting stays
on the caller at log-time; the per-transport mailbox and WritableStream queues
keep their existing depths.

### Measured effect

`winston/benches/logger_benchmark.rs` (criterion), comparing this change against
the prior `RwLock` snapshot as the baseline:

| benchmark | change |
| --- | --- |
| `logger_overhead/noop_transport` (1000 logs + flush) | **−18%** |
| `multi_threaded/1` | **−22%** |
| `multi_threaded/4` | −1% (neutral) |
| `message_size/100` | −19% |

The win is the eliminated per-call snapshot allocation. Multi-producer contention
(`multi_threaded/4`) is unchanged — the slot list is read-shared either way.

## Alternatives considered and rejected

Two larger reworks were prototyped alongside the arc-swap change and **rejected on
measurement**. Both are recorded here because each is a plausible idea that the
benchmark refuted.

### Finalize at the pump ("format-move")

Run only the format's *transform* stage on the caller and move the *finalizer*
(the string render) into the per-slot pump task. The appeal: render off the
caller, parallel across transports, and skipped for entries dropped at the
mailbox.

Rejected — it regressed the common paths and bought no correctness:

- **Single transport:** +12% to +23%. Render-on-caller is a 3-stage pipeline
  across 3 threads (caller renders, pump enqueues, controller writes); moving the
  render into the pump collapses render+enqueue onto one thread.
- **Multi-producer (`multi_threaded/4`): +100%+** regardless of WS HWM. Several
  caller threads rendering in parallel become one pump thread rendering serially
  — the render funnels through a single task.
- **No correctness gain.** The reason to fix formatting timing is that a
  time-dependent transform (`timestamp`, `ms`) must be evaluated once, at
  log-time, so every transport observes the same value. That comes from running
  *transforms* at log-time — which render-on-the-caller already does, because it
  runs the whole pipeline there. Finalize-at-pump was a pure performance bet, and
  it lost.

### Single buffer / shallow WS high-water mark ("kill the 2×")

> **Superseded by [ADR 0007](0007-shallow-writablestream-high-water-mark.md).**
> The rejection below rests on a burst-then-flush benchmark whose fixed 1000-entry
> burst manufactures a false "deeper is better" plateau. A purpose-built
> *sustained* benchmark reversed it: shallow WS is the throughput optimum, the
> knee is an absolute cache-fit count (not a fraction of capacity), and the WS HWM
> is now `min(capacity, 64)`. See `docs/queue-depth-investigation.md`. The analysis
> below is retained as the record of the artifact-laden first read.

Per transport there are two bounded buffers in series — the mailbox (capacity N,
bearing `OverflowPolicy`) and the WritableStream's internal queue (HWM also N).
Shrinking the WS HWM to a small fixed depth would cut worst-case in-flight from
~2N to ~N.

Rejected — the WS queue depth is load-bearing for throughput. An HWM sweep in the
final (render-on-caller + arc-swap) configuration, vs the same baseline:

| WS HWM | `noop` | `multi/1` | `multi/4` |
| --- | --- | --- | --- |
| 1024 (= capacity) | −18% | −22% | −1% |
| 256 | −16% | −20% | −2% |
| 64 | −11% | −14% | **+5%** |
| 16 | −8% | −9% | −3% |

Bigger is monotonically better across the swept range. The depth amortizes the
**cross-thread pump↔controller handoff**: each `write()` is an `unbounded_send`
that wakes the controller task; a deep queue lets the pump enqueue a batch and the
controller drain a batch per wakeup, instead of a wakeup every entry. The
`whatwg_streams` writer confirms this shape — `enqueue_when_ready` awaits the
`ready()` (HWM) gate, then fires `write()` without awaiting the sink; backpressure
is the `ready()`/`desired_size` mechanism, decremented only after each
`sink.write().await` completes.

The apparent flattening near 1024 is **a property of this benchmark, not a
universal plateau**, and the sweep does not actually establish one: the HWM was
clamped at `capacity` (default 1024) during the sweep, so HWM > capacity was never
measured; and the benchmark logs a fixed 1000-entry burst per iteration, so once
the HWM exceeds the backlog the pump can build ahead of the controller (≈ the
burst size), the pump stops parking entirely and throughput tops out. The "enough"
depth therefore tracks the workload — burst size, how far the producer outruns the
sink — not a fixed constant. What the sweep does establish is the direction:
shallow HWM costs throughput.

The 2× it would save is **worst-case, under sustained saturation only** — when the
sink can't keep up, the WS queue fills to N, then the mailbox fills to N, then the
caller hits its `OverflowPolicy`. In steady state both queues sit near-empty, so
the 2× is a memory *ceiling*, not typical occupancy. Buying that ceiling back
costs 7–10% on the always-on hot path: a bad trade for a logger.

So the WS HWM stays equal to the mailbox capacity. The rule is uniform and
scale-invariant: **capacity N ⇒ up to ~2N in-flight, worst-case**.

#### Sub-option: cap the WS HWM (`min(capacity, X)`)

A cap would help only if the WS queue's throughput benefit truly plateaus at some
depth `X` while its memory cost keeps growing linearly — then for a very large
`capacity` the WS depth above `X` would be pure memory for no throughput return.

Deferred, and **not for a small reason**: there is no measured `X`. The benchmark
that suggested a plateau logs a fixed burst, so its apparent knee sits at ≈ the
burst size and moves with the workload (see above); the sweep also never measured
HWM > capacity. So a cap would be encoding a guess. Picking `X` honestly needs a
benchmark the current one can't provide — sustained, continuous logging against a
rate-limited sink, sweeping HWM independently of `capacity` — to see where (or
whether) the diminishing-returns curve actually flattens. Until then the uniform
`WS_HWM = capacity` rule stands, and the cap is a future change gated on that
measurement, not on taste.

## Consequences

- **`log()` is lock-free and allocation-free on the slot list** — one atomic
  `Arc` load replaces an `RwLock` read plus a `Vec`/`String`/`HashMap` clone.
- **Admin operations rebuild and publish a `Routing`** under a dedicated mutex,
  rather than mutating a locked `LoggerState` in place. They clone the slot list
  on each mutation; this is off the hot path and admin operations are rare.
- **No behavior change.** Dispatch order, level gating, per-slot formatting, the
  `FormattedEntry` boundary, and the ~2× per-transport buffering are exactly as
  before. This ADR is a storage/locking change, not a semantic one.
- The format-move and single-buffer reworks are **not** adopted; this file records
  why so they aren't re-proposed without new evidence.

## References

- `winston/src/pipeline.rs` — `Routing`, `build_slot`, `dispatch_entry`.
- `winston/src/logger.rs` — `ArcSwap<Routing>`, `admin_lock`, admin operations.
- `winston/benches/logger_benchmark.rs` — the benchmark behind the numbers above.
- ADR 0002 — Direct-dispatch backpressure (the dispatch model this preserves).
- ADR 0005 — LogInfo data model and the FormattedEntry boundary (the format
  boundary this preserves).
