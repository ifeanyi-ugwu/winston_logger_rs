# ADR 0002 — Direct-dispatch backpressure

**Status:** Accepted

## Context

The Logger's front end is currently a chain of four queues between `log()` and
the sink:

```text
caller crossbeam channel (bounded, channel_capacity, BackpressureStrategy)
  → bridge thread
  → pipeline fmpsc channel (UNBOUNDED)
  → fanout task
  → per-slot mailbox (bounded, queue_capacity, OverflowPolicy)
  → per-slot pump
  → WritableStream queue (bounded by HWM)
  → sink
```

This shape was assembled incrementally — the bridge + pipeline channel exist to
hand off from sync `log()` to the async fanout; the caller channel + strategy
were already there before the streams refactor; the per-slot mailbox is what
made per-transport backpressure work at all (added in `f59ccca`).

Two things about this chain are wrong:

1. **The pipeline channel is unbounded.** Under sustained slow-sink pressure
   the mailbox fills → fanout's `send.await` parks → pipeline channel grows
   without bound. Memory leaks until OOM, and the bridge never blocks on
   `unbounded_send`, so the caller channel never fills either.

2. **`BackpressureStrategy` therefore never fires under normal logging.** The
   only window for caller-side overflow is a producer thread outrunning the
   bridge thread (sub-µs consecutive `log()` calls), which is essentially a
   scheduler accident. Users configure `BackpressureStrategy::Block` or
   `DropOldest` thinking they're picking a policy, but in practice the system
   does whatever it does — the strategy is decorative. The five `#[ignore]`d
   tests in `winston/tests/backpressure.rs` are evidence: they assumed the
   pre-refactor model where the caller channel was the only pressure point, and
   none of them reliably pass on the current architecture because that's not
   how pressure flows anymore.

The real control surface — the one that fires reliably and matters — is the
per-transport `OverflowPolicy` (Block / DropNewest / DropOldest) on each slot.
A user who wants "block on file durability, drop on console freshness" cannot
express that with the caller-side strategy at all; they need per-slot policies,
which the architecture has.

So the front-end machinery isn't load-bearing for backpressure; it's load-
bearing only for "decouple the sync caller from the async runtime." That
decoupling is real, but the cost of it is the architectural lie above, plus
configuration surface (`channel_capacity`, `BackpressureStrategy`) that doesn't
do what its names suggest.

Industry precedent: synchronous-dispatch loggers (`zap`, `zerolog`,
`tracing` core, Python stdlib `logging`) all treat slow writers as direct
backpressure on the caller. `slog-async` is the "buffer + drop policy" model
and is well-known for its tuning headaches.

## Design principle

The model we want is **water through pipes**: each transport is its own pipe;
each pipe has a fixed capacity; the slowest pipe sets the rate the producer
can flow at; if you don't want a particular pipe to gate the producer, you
put a "drop overflow" valve on *that* pipe. Configuring this should not be
a headache — one decision per transport, that decision actually does what
it says, no global buffer-tuning math.

Two corollaries follow that this ADR's rejected alternatives all violate:

- **Unbounded anywhere = the lie returns.** Any buffer between `log()` and a
  sink that can grow without bound turns "Block" into "Drop later, silently,
  when memory runs out." `BackpressureStrategy`'s failure mode in the current
  code is exactly this; reintroducing an unbounded queue in any other shape
  (unbounded crossbeam between log() and dispatcher, the existing `fmpsc::
  unbounded` pipeline channel, etc.) reproduces the same problem.
- **One configuration surface per transport, or none.** Layered configuration
  (caller-side knob *and* per-transport knob) forces users to reason about the
  interaction. With one queue per transport and one policy per transport,
  there's no interaction to reason about.

## Decision

Replace the four-queue chain with **direct synchronous dispatch into per-slot
mailboxes**, with the per-slot `OverflowPolicy` as the *only* knob users
configure for pressure behavior.

### Removed

- `BackpressureStrategy` enum and its `Block` / `DropOldest` / `DropCurrent`
  variants.
- `channel_capacity` knob on `LoggerOptions` / `LoggerBuilder`.
- The crossbeam caller channel (`Sender<LogMessage>` / `Receiver<LogMessage>`)
  and the `LogMessage` enum.
- The bridge thread (`bridge_loop`, `bridge_thread`) entirely.
- The unbounded `fmpsc::UnboundedSender<PipelineMessage>` pipeline channel.
- The fanout task. Its state (slot list, format/level config, stats map, event
  senders) moves directly into the `Logger`, behind a single mutex for admin
  operations (add / remove / configure) that mutate the slot list.
- `PipelineMessage::Entry` / `PipelineMessage::Flush` (admin variants become
  direct method calls on the Logger; no message-passing for entries).

### Kept

- The per-slot mailbox (the custom SPSC in `winston/src/mailbox.rs`), now the
  only buffer in the front end.
- The per-slot pump task, unchanged.
- The per-slot `WritableStream` + writer + sink, unchanged.
- Per-transport `OverflowPolicy` and `with_queue_capacity(n)` — now the
  *only* user-facing pressure controls.
- Per-transport `TransportStats` (counters) and `subscribe_backpressure`
  (events) — both already report at the only place pressure actually lives.

### Added

- `MailboxSender::push_blocking(msg)` — sync, parks the calling thread on a
  `parking_lot::Condvar` when the mailbox is full and the slot's policy is
  `Block`. Notified by the receiver on every successful pop.
- `Logger::log()` dispatches synchronously: level filter → for each slot,
  format → `mailbox.try_push` → on Full, apply the slot's policy (`Block` →
  `push_blocking`; `DropNewest` → drop + count + emit edge event; `DropOldest`
  → `force_push_dropping_oldest` + count + emit edge event).
- Admin operations (`add_transport`, `remove_transport`, `configure`, `flush`,
  `close`) become direct method calls that mutate the Logger's slot list under
  a short-lived write lock and send admin `SlotMessage` variants
  (`Flush(ack)` / `Close`) through each slot's mailbox.

### Choice of mailbox primitive

The sync→async handover lives entirely inside the per-slot mailbox: sync push
on the producer side (caller thread executing `log()`), async pop on the
consumer side (per-slot pump task awaiting writer.enqueue_when_ready). The
question that drove this section is: can that handover be made smoother than
"caller thread acquires a per-slot mutex" — specifically, can the per-slot
hot path be lock-free, so concurrent callers don't serialise on the slot's
mutex?

Three primitives are real candidates. None reintroduces a global front-end
queue (that would re-create the architectural lie this ADR rejects); they
differ only in how the per-slot mailbox itself is implemented:

- **`crossbeam::queue::ArrayQueue` + `Parker`/`Unparker`** — lock-free push, but
  doesn't natively support `force_push_dropping_oldest` (we'd fake it with
  try_push + try_pop + try_push, race-tolerant only because we're SPSC at the
  producer side). More moving parts.
- **`flume::bounded`** — gives sync `send` + async `recv` on the same channel,
  no glue code needed. But adds a dep, and we'd still wrap drop-oldest
  ourselves. Reasonable if the contention argument below ever bites.
- **Our custom mailbox** — already built, already correct shape, already
  supports `force_push_dropping_oldest` natively, zero deps.

The contention cost (per-slot `parking_lot::Mutex`) is bounded:
~10–50ns per uncontested push, scaling with concurrent producers on the same
slot. Negligible at typical logging rates (1k–100k entries/sec). Only matters
at unusual scales (10M+ entries/sec on hundreds of concurrent producer
threads), at which point the per-slot primitive can be swapped behind the
existing public interface without touching the Logger.

## Operating rules (what the new contract means)

1. **`logger.log()` is synchronous and may block.** Under sustained pressure
   from a `Block`-policy slot whose sink is slow, the calling thread blocks on
   that slot's mailbox until room appears. This is the *point* of `Block`;
   document it loudly.

2. **The slowest `Block`-policy slot sets the global rate.** Multiple `Block`
   slots compound serially within a single `log()` call (each slot is pushed
   in turn; a saturated one parks). Mix `Block` (durability lanes — file,
   daily-rotate) with `DropNewest` / `DropOldest` (telemetry lanes — HTTP,
   Mongo, console) when you want fast lanes to stay fast.

3. **Drop policies never block.** A saturated `DropNewest` slot drops the new
   entry; `DropOldest` evicts the head and pushes the new one. In both cases
   `log()` returns immediately for that slot; `dropped_total` ticks; an
   edge-triggered `Saturated` event fires.

4. **One bounded buffer per transport, period.** No global front-end buffer.
   Memory is exactly `Σ(queue_capacity[i])` plus the WritableStream queue per
   slot (also bounded by HWM, which we set to `queue_capacity`).

5. **Flush is per-slot, joined.** `logger.flush()` sends a `SlotMessage::Flush`
   barrier through each slot's mailbox (sync `push_blocking` to guarantee
   delivery even when saturated), then awaits every pump's `writer.flush()`
   ack. Same semantics as before; routing changed.

6. **Close is per-slot, parallel-joined.** `logger.close()` sends
   `SlotMessage::Close` to each slot, awaits each pump's exit. The pump runs
   `writer.close()` (which drives `WritableSink::close`) before signalling
   done — same drain-and-flush-buffers lifecycle as before.

## Alternatives considered and rejected

- **Bound the pipeline channel.** Closest "minimal patch" that fixes the
  unbounded-growth problem: replace `fmpsc::unbounded` with `fmpsc::channel(n)`
  and have the bridge use `block_on(pipeline_tx.send(msg))`. Restores
  end-to-end propagation, lets `BackpressureStrategy` fire as advertised. But
  it preserves the four-queue chain, the bridge thread, and a configuration
  surface (now *two* capacities and two policies) that's harder to reason
  about than the per-transport model. Fixes the lie without removing it.

- **Keep `BackpressureStrategy` as a vestigial knob.** Quiet API erosion is
  worse than a clean break. Users who configured it deserve to know that
  what they configured doesn't fire, not to have us pretend forever.

- **Unbounded crossbeam queue between `log()` and dispatch.** Re-introduces
  the exact lie we just identified: producer never blocks, buffer grows
  without bound, "Block" means nothing.

- **Async `log()` API (`async fn log`).** Makes the sync-async impedance
  go away, but breaks every sync call site (which is most of them — `log`
  crate adapter, all of `println!`-style usage). Wrong trade.

- **`block_on(async_send)` inside `log()` per call.** Spins up a tiny
  executor (or pins a static one) per call. Performance hit per `log`, and
  exposes the user's choice of runtime to subtle interactions with the
  static one. Worse than the per-slot mutex's contention cost.

- **`flume` per slot today.** Reasonable; deferred. Our custom mailbox does
  the same job at zero dep cost, and swapping the primitive behind the
  mailbox's public surface is straightforward if the contention argument
  ever bites.

- **`crossbeam::queue::ArrayQueue` per slot today.** Same reasoning as
  `flume`. The lock-free push is nice but we'd still need to layer
  drop-oldest and parking on top.

- **Keep the fanout task; let it own the slot list.** The fanout task only
  existed to be the async-side counterpart of the bridge. With the bridge
  gone, a single task that owns mutable state behind an mpsc channel of
  admin messages is just a mutex with extra steps. Move the state to the
  Logger; use a short-lived `RwLock` write for admin operations; read it
  lock-free for `log()` (or behind a per-call read lock).

## Consequences

- **Single configuration knob per transport** — `OverflowPolicy` and
  `queue_capacity`. No global front-end tuning. Default is
  `OverflowPolicy::Block` and `queue_capacity = 1024` per transport; users who
  want fast lanes opt those transports into a drop policy.

- **`log()` blocks under saturated Block.** The right semantics for "I picked
  Block; honour it." Loud in the `OverflowPolicy::Block` doc-comment.

- **No more unbounded growth anywhere.** Total in-flight memory is
  `Σ_i (queue_capacity[i] + HWM[i])` ≈ `2 · Σ_i queue_capacity[i]`. Bounded by
  construction.

- **No more bridge thread, no more fanout task.** One std-thread and one
  per-Logger spawned task removed.

- **Burst absorption moves from "front of caller channel" to "front of each
  slot mailbox."** A 1000-entry burst hits each mailbox directly rather than
  landing in a large caller channel and trickling down. For Drop slots this
  shows up as drops on the burst; for Block slots it shows up as the producer
  feeling pressure. This is the correct behaviour; today's behaviour is "burst
  invisibly accumulates in the unbounded pipeline channel."

- **Concurrent log() callers contend on per-slot mutex.** `parking_lot`
  uncontested cost is ~ns; contention scales with concurrent producers on the
  same slot. Acceptable up to low-hundreds of concurrent producers per slot at
  typical logging rates. Lock-free primitive (crossbeam / flume) is a drop-in
  upgrade behind the mailbox surface if it ever matters.

- **Breaking API change.** `BackpressureStrategy`, `channel_capacity`,
  `LoggerBuilder::backpressure_strategy`, and `LoggerOptions::backpressure_strategy`
  are removed. The crate is pre-1.0 (`0.8.x-dev`); migration is a one-line
  delete per `Logger::builder()` call site (logmark, examples, user code).
  Users who configured `Block` see no behaviour change beyond `log()` now
  reflecting real pressure; users who configured `DropOldest` / `DropCurrent`
  do see a behaviour change (their drops become blocks under per-slot Block,
  unless they reconfigure that transport's `OverflowPolicy` to a drop variant).
  This is the correct correction.

- **`winston/tests/backpressure.rs` deleted.** Its five `#[ignore]`d tests
  were asserting against the pre-refactor model. The new model's per-slot
  policies are covered by `test_drop_oldest_evicts_head_and_delivers_newest`
  and the event-transition tests in `logger.rs`; the caller-side strategy no
  longer exists to test.

## References

- `winston/src/mailbox.rs` — the per-slot SPSC mailbox primitive.
- `f59ccca` — original introduction of per-slot pump + `OverflowPolicy`.
- `76e1181` — earlier cap-defeat bugfix that exposed the per-slot bound was the
  effective control.
- `22ab267` — replacement of `fmpsc::channel` with the custom mailbox.
- ADR 0001 — Cross-transport proxying (the per-transport handle model this ADR
  builds on).
