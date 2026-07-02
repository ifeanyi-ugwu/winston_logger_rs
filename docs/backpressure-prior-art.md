# Prior art — how mature loggers handle the sync-call backpressure tension

**Status:** Reference / comparative survey. Companion to
[mailbox-concurrency-investigation.md](mailbox-concurrency-investigation.md) —
specifically its impossibility triangle (sync call site / no thread-park /
lossless: pick two).

## How to read this

winston's async-door question is whether a *synchronous* `log()` call can get
*lossless* backpressure without *parking a thread*. This page surveys how the
mature loggers answer the same tension. The finding, up front:

- They all resolve the triangle by **picking a corner**, exactly as winston does.
- Every "non-lossy" / blocking option in the survey is **thread-parking** — the
  same move as winston's sync `OverflowPolicy::Block`. None offers an
  async-cooperative `.await` door.
- The reason the block rarely bites them is not a cleverer mechanism; it is *who
  owns the blocking thread* (a dedicated OS thread, or a forgiving runtime), plus
  a lossy-by-default convention.

Facts are verified against version-pinned docs/source; versions and URLs are in
each entry and collected under [References](#references). Verified 2026-07.

## The shared pattern

Nearly every "async" logger is the same shape winston built:

```text
sync log!() ─► bounded queue ─► background worker thread ─► sink
                     ▲
             overflow policy here: drop, or block the caller
```

`log!()` stays synchronous because all it does is *hand off* to the queue; the
writing happens on the worker. That is winston's `log()` → per-slot mailbox →
pump, exactly. So "lossy vs non-lossy" is not a different mechanism from
winston's — it is *which corner of the triangle the queue-full case takes.*

## Rust

### `tracing` + `tracing-appender` (tracing-appender 0.2.5)

The closest analog to winston in Rust.

- **Core `tracing` is synchronous on the calling thread.** `Subscriber::event`
  runs inline at the macro callsite (`&self`, no deferral), and
  `tracing_subscriber::fmt` performs the write on the recording thread under a
  lock — no background queue by default. So plain `tracing` is the "block on the
  write" corner.
- **`tracing_appender::non_blocking`** adds the queue: a bounded
  `crossbeam_channel` (`buffered_lines_limit`, default **128_000**) drained by a
  dedicated worker OS thread, with a `WorkerGuard` that flushes on shutdown.
- **`lossy` defaults to `true`.** In lossy mode it uses `try_send` and **drops**
  on a full queue, incrementing an internal saturating drop counter — that count
  is *not* emitted to the sink automatically. In `lossy(false)` it uses a
  **blocking** `crossbeam` `send` that **parks the calling thread** until capacity
  frees.
- The naming tell: it is called `non_blocking`, and that is true only in the
  default lossy mode. Flip it to lossless and the caller blocks — winston's sync
  `Block`, verbatim. There is no async-cooperative mode.

### `slog` + `slog-async` (slog-async 2.8.0)

- **Thread-based**, same shape: `slog_async::Async` sends records over a bounded
  channel (`chan_size`, default **128**) to a dedicated worker thread that runs
  the wrapped inner drain.
- **`OverflowStrategy`** has three usable variants: `DropAndReport`, `Drop`,
  `Block`. The **default is `DropAndReport`** (drop on full and count it), *not*
  block. `Block` parks the calling thread until there is room.
- The docs explicitly caveat that overflow handling is "implementation defined,
  might change and should not be relied on," and the enum is non-exhaustive by
  design — a useful reminder that even the mature libraries treat this boundary as
  soft.

### `log` + `env_logger` (env_logger 0.11.11)

- **No queue at all.** `env_logger`'s `Log::log` formats and writes on the calling
  thread, holding the `Stdout`/`Stderr` stream lock (a custom pipe target is
  wrapped in a `Mutex`). The caller blocks on the write itself.
- No lossy/drop option exists — pure "block on the write" corner. It never
  promised non-blocking, so it has no backpressure problem to solve.

## Go

### `go.uber.org/zap` (zap 1.28.0)

- **Synchronous** on the calling goroutine (encode + write to the `WriteSyncer`),
  no default background queue.
- **`BufferedWriteSyncer` is batching, not overflow-dropping**: it buffers writes
  and flushes on size (default **256 kB**) or interval (default **30s**),
  whichever first — to cut syscalls. It does not drop on overflow.
- No bounded-queue drop mechanism. Zap's only intentionally lossy feature is
  **sampling** (`NewSamplerWithOptions`): keep the first N entries per
  level+message per tick, then every Mth, drop the rest — observable via
  `SamplerHook`. That is a *different* kind of lossy (deduplication under
  duplicate-heavy load), not queue-overflow backpressure.

### `github.com/rs/zerolog` (zerolog 1.35.1)

- **Synchronous by default** — writes on the calling goroutine.
- **`diode.Writer`** is the opt-in Drop corner: a thread-safe, lock-free,
  non-blocking ring buffer (wrapping `code.cloudfoundry.org/go-diodes`) that
  **never blocks producers and drops on overflow**, invoking an `Alerter`
  callback with the number of missed messages. Signature:
  `NewWriter(w io.Writer, size int, pollInterval time.Duration, f Alerter) Writer`,
  `Alerter = func(missed int)`.

## Why the block rarely bites them — the one real difference

The architecture is the same everywhere; what differs is *who owns the thread that
`Block` parks*, and whether that is affordable:

| logger | who runs the worker | whose thread `Block` parks | affordable? |
| --- | --- | --- | --- |
| tracing-appender / slog-async | a dedicated OS thread | the **caller's** thread | yes *if* the caller is a normal app thread; **no if the caller is an async task** — it parks that runtime's worker |
| zap / zerolog (Go) | calling goroutine (+ optional flush goroutine) | a goroutine | yes — the Go runtime hands the P to another thread, so other goroutines keep running |
| winston, under an async runtime | async pump task on the user's runtime | the caller's **executor worker** | no — cooperative scheduling has no preemption |

Two mechanisms make the Go column affordable, and neither is available to Rust
async:

- **Channel/mutex block:** the goroutine parks (`gopark` → `_Gwaiting`) and its M
  keeps its P and immediately runs another runnable goroutine — pure user-space
  reschedule, no OS thread blocked.
- **Blocking syscall (e.g. a slow write):** the M blocks in the kernel, but the
  runtime detaches its P (`entersyscall` → `_Psyscall`) and hands it to another M
  (`handoffp`, backed by the `sysmon` monitor), so the P's other goroutines keep
  running. (The often-quoted "10 ms" threshold is for preempting a long-*running*
  goroutine, not for this syscall handoff — do not conflate them.)

So the honest reading of "mature libraries don't seem to have winston's block": it
is not a better design — it is that (a) usage is **lossy by convention**, and
(b) their blocking parks a *dedicated or forgiving* thread rather than a
cooperative executor worker. Call `tracing`'s lossless writer from inside a
`tokio` task and let it saturate, and it parks the tokio worker — **winston's
exact problem.** They simply default lossy and target normal-thread callers, so
nobody points at it.

## What this means for winston

- **The pattern is standard, not a deviation.** Per-transport bounded queue +
  background pump + per-overflow policy is the mature shape;
  [ADR 0002](adr/0002-direct-dispatch-backpressure.md) arrived at it independently
  and names slog-async's model directly.
- **The async-cooperative door is beyond the common design.** Nowhere in the
  survey does "lossless" mean anything but "block a thread." So a correct
  `log_async().await` (see the mailbox investigation's fork C) is a step *past*
  the ecosystem, into the async-runtime-integrated, must-not-drop niche — not a
  catch-up.
- **The ecosystem defaults lossy on purpose**, and that validates confining the
  `.await` to the must-not-drop lane: tracing-appender defaults `lossy=true`,
  slog-async defaults `DropAndReport`, zerolog's diode is opt-in drop, zap's only
  overflow-ish feature is sampling. Logs are treated as best-effort observability
  — drop under extreme load rather than slow the application.

## References

- tracing — `Subscriber::event` (synchronous dispatch):
  <https://docs.rs/tracing/latest/tracing/subscriber/trait.Subscriber.html>
- tracing-subscriber — `MakeWriter` (IO on the recording thread):
  <https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/trait.MakeWriter.html>
- tracing-appender 0.2.5 — `non_blocking` source (channel, lossy try_send/blocking
  send, drop counter):
  <https://docs.rs/tracing-appender/0.2.5/src/tracing_appender/non_blocking.rs.html>
- tracing-appender 0.2.5 — worker thread source:
  <https://docs.rs/tracing-appender/0.2.5/src/tracing_appender/worker.rs.html>
- tracing-appender — `NonBlockingBuilder` docs (`lossy` default, backpressure
  note, `DEFAULT_BUFFERED_LINES_LIMIT = 128_000`):
  <https://docs.rs/tracing-appender/latest/tracing_appender/non_blocking/struct.NonBlockingBuilder.html>
- slog-async — `OverflowStrategy` (variants, `DropAndReport` default, `Block`
  semantics): <https://docs.rs/slog-async/latest/slog_async/enum.OverflowStrategy.html>
- slog-async — source (`chan_size` default 128, worker thread):
  <https://github.com/slog-rs/async/blob/master/lib.rs>
- env_logger — source (`impl Log`, synchronous write, stream lock):
  <https://github.com/rust-cli/env_logger/blob/main/src/logger.rs>
- env_logger — docs overview: <https://docs.rs/env_logger/latest/env_logger/>
- zap — `zapcore` (`BufferedWriteSyncer` 256 kB / 30s; `Sampler`):
  <https://pkg.go.dev/go.uber.org/zap/zapcore>
- zap — `Sampler` source: <https://github.com/uber-go/zap/blob/master/zapcore/sampler.go>
- zerolog — package docs: <https://pkg.go.dev/github.com/rs/zerolog>
- zerolog — `diode.Writer` (lock-free ring buffer, drops on overflow, `Alerter`):
  <https://pkg.go.dev/github.com/rs/zerolog/diode>
- Go scheduler — Dmitry Vyukov, *Scalable Go Scheduler Design Doc* (P handoff /
  sysmon origin):
  <https://docs.google.com/document/d/1TTj4T2JO42uD5ID9e89oa0sLKhJYD0Y_kqxDv3I3XMw/edit>
- Go scheduler — Daniel Morsing, *The Go scheduler* (syscall handoff):
  <https://morsmachine.dk/go-scheduler>
- Go runtime — `runtime/proc.go` (`gopark`, `handoffp`, `sysmon`, `retake`):
  <https://go.googlesource.com/go/+/refs/heads/master/src/runtime/proc.go>
</content>
