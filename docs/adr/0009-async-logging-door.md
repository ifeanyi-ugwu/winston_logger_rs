# ADR 0009 — Async logging door (cooperative backpressure)

**Status:** Accepted — phased rollout, not yet implemented (see [Rollout](#rollout-phased)).

Adds the opt-in async path that [ADR 0002](0002-direct-dispatch-backpressure.md)
scoped out. 0002 rejected *replacing* sync `log()` with `async fn log`; this ADR
*adds* `log_async` alongside it and leaves the sync path — and all of 0002 —
unchanged. The reasoning trail (the impossibility triangle, the route comparison,
the half-built primitive) is
[docs/mailbox-concurrency-investigation.md](../mailbox-concurrency-investigation.md);
the ecosystem survey is
[docs/backpressure-prior-art.md](../backpressure-prior-art.md).

## Context

Under `OverflowPolicy::Block`, sync `log()` parks the calling thread on the
per-slot condvar when the mailbox is full (ADR 0002). On a normal application
thread that is correct backpressure. On an **async runtime** it is not: the caller
is a task on a shared executor worker, and `push_blocking` has no yield point a
cooperative scheduler can preempt — so parking it steals a whole worker, and on a
single-threaded runtime deadlocks (the parked thread is the one that would drain
the mailbox). An async application that wants *lossless* backpressure (Block,
must-not-drop) therefore has no good option today: Drop policies never block but
lose entries; Block blocks the wrong thing.

The choice is structural, not an implementation gap. Three properties —
**synchronous call site**, **no thread-park**, **lossless** — cannot all hold at a
saturated buffer (infinite inflow, finite outflow, finite memory, no block, no
drop is a contradiction). Sync `Block` gives lossless + sync but parks; Drop gives
sync + no-park but lossy; the missing corner — lossless + no-park — requires
yielding the *task*, which is an `.await` at the call site. The `.await` is the
mechanism of cooperative backpressure, not incidental ergonomics.

No mainstream logger fills this corner. `tracing-appender`, `slog-async`, `zap`,
and `zerolog` all resolve the same tension by picking a corner, and every one of
their "lossless" options is thread-parking; they get away with it because the
blocking lands on a *dedicated* OS thread (Rust) or a runtime that hands the P off
(Go). So this door is additive beyond the ecosystem norm, for the
async-integrated, must-not-drop niche (see `docs/backpressure-prior-art.md`).

The mailbox already carries a half-built async producer path (`send()` +
`producer_waker`), but it is dead (no caller) and multi-producer-unsafe: the
single-slot `AtomicWaker` clobbers registrations when two async producers wait on
one mailbox, a latent lost-wakeup. Building the door requires making that path
correct first.

## Decision

Add an opt-in async logging door, behind a cargo feature, built on a correct
multi-producer async wait. The sync path and every decision in ADR 0002 stay
exactly as they are.

### The primitive (the mailbox waiter)

Replace the mailbox's single-slot `producer_waker: AtomicWaker` — the dead,
MP-unsafe field — with a fair multi-waiter primitive backed by
[`event-listener`](https://docs.rs/event-listener)'s `Event`. `send()` becomes a
small async loop:

```rust
pub async fn send(&self, mut msg: T) -> Result<(), SendError<T>> {
    loop {
        match self.try_push(msg) {
            Ok(())          => return Ok(()),
            Err(Closed(m))  => return Err(SendError(m)),
            Err(Full(m))    => msg = m,
        }
        let listener = self.inner.room.listen();
        // re-check after listen() so a notify between the failed try_push
        // and the listen() is not missed
        match self.try_push(msg) {
            Ok(())          => return Ok(()),
            Err(Closed(m))  => return Err(SendError(m)),
            Err(Full(m))    => msg = m,
        }
        listener.await;
    }
}
```

The receiver `notify(1)`s on every pop and `notify(usize::MAX)` on drop. This is
wake-one, cancellation-safe, and free of the stale-waker hazard a hand-rolled
`VecDeque<Waker>` has (a re-polled future can enqueue duplicate wakers; waking a
stale one leaves a live waiter unwoken). `event-listener` owns the waiter set and
handles registration, fair single-wake, and removal-on-drop — the exact primitive,
proven in `async-channel` and `async-lock`.

It lives behind an **`async-log` cargo feature**, so a build that does not use the
door keeps ADR 0002's zero-dependency mailbox. The condvar path
(`push_blocking`) is untouched and remains the sync producer.

### The dispatch method

`Logger::log_async(&self, entry) -> impl Future<Output = ()>` — the async twin of
`dispatch_entry`:

- **Phase 1**, unchanged from sync: `try_push` (non-blocking) to every
  level-admitted slot; Drop-policy slots resolve here exactly as today.
- **Phase 2**: `join` the per-slot `send().await` for the `Block`-full slots —
  parallel waiting, the async incarnation of
  [ADR 0003](0003-parallel-deferred-block-dispatch.md), so one slow slot does not
  serialise the rest. Each awaits *per-slot* room; slots stay independent, no
  shared upstream queue.

### The surface

- `global::log_async(entry).await`, mirror of `global::log`.
- `create_async_level_macros!` generating `info_async!` … `trace_async!`, each
  expanding to a **visibly-awaited** `async {}` block that preserves the lazy
  level-gate (nothing is built when the level is disabled). The `.await` stays at
  the call site — it is a suspend and cancellation point, and hiding it inside a
  statement-shaped macro would misrepresent it.

Naming is the `_async` suffix across method, global fn, and macros; the underlying
method is `log_async` in all cases.

### Kept unchanged

Sync `log()`, `push_blocking`, the condvar, per-slot `OverflowPolicy`,
`dispatched_total` / `dropped_total`, and the whole of ADR 0002. With `async-log`
off, the library is byte-for-byte the current one.

## Operating rules — the cancellation contract

Published on `log_async` (and the macros). The async lossless lane owes its users
the one precise way a log can still be lost.

1. **It awaits room, never the write.** `log_async` completes when the entry is
   handed to the mailbox, not when the sink writes it — identical to sync. An
   un-backpressured entry is pushed on the first poll (no suspension); a
   backpressured one yields the task and resumes when a slot frees.
   Await-until-durable is `flush()`, deliberately not on this path.

2. **Lossless under backpressure, not under cancellation.** The awaiting future
   holds the entry; if the task is cancelled (the future dropped — `select!`,
   `timeout`, `abort`, a dropped request task) *while backpressured*, that entry is
   lost. The risk exists **only** while the target mailbox is full; un-backpressured
   entries cannot be cancel-lost.

3. **Cancellation loss is counted, not silent.** A dedicated per-transport
   `cancelled_total` counter — distinct from `dropped_total` (policy drops) —
   increments via an armed drop-guard around each per-slot await, disarmed on
   success. No `BackpressureEvent` fires: that signal marks saturation
   *transitions*, and a cancellation is not one.

4. **Fan-out is best-effort per transport.** On cancellation the entry stays in
   every transport that already accepted it and is dropped for those still awaiting
   room — partial delivery across transports is possible. It is not all-or-nothing:
   holding the entry out of every mailbox until all can accept would need a
   cross-mailbox two-phase commit, reintroducing exactly the head-of-line coupling
   the per-slot design removed.

5. **No retry handle.** A cancelled `log_async` does not return the entry.

## Alternatives considered and rejected

- **Delete the dead async path (fork A).** The honest default while no async
  must-not-drop caller exists. Superseded by the decision to build one.
- **Fence the dead path (fork B).** Doc it single-producer-only and debug-assert a
  single registrant. An interim, not an endpoint; skipped now that the path is
  being built correctly.
- **Single dispatcher / shared front queue (Route 2).** A lone dispatcher task as
  the sole producer would make the `AtomicWaker` correct by construction, sidestepping
  the primitive fix. Rejected: a *shared upstream* queue reintroduces head-of-line
  blocking (one slow `Block` sink stalls the dispatcher, backing up every lane) and
  cannot express per-transport `OverflowPolicy` at its own boundary — the exact
  coupling ADR 0002 removed. Per-transport front queues to avoid that just re-create
  the mailboxes with an extra hop.
- **Hand-rolled zero-dependency waiter queue.** Correct but ~40–50 lines of subtle
  concurrency (stale-waker suppression, cancel-mid-wait removal) needing thorough
  tests. `event-listener` is smaller, proven, and cancellation-safe; the feature
  gate keeps the dependency off the default build.
- **Wake-all zero-dependency queue.** Simple and correct, but a thundering herd —
  O(K) task wakes per freed slot under sustained backpressure. Once a dependency is
  accepted, `event-listener` gives wake-one for the same effort.
- **Replace sync `log()` with `async fn log`** (ADR 0002's rejected alternative).
  Breaks every sync call site — the `log`-crate adapter, all `println!`-style
  usage. This ADR adds an async variant *alongside*; it does not replace.
- **`.await` hidden inside the macro.** A statement-shaped `info_async!(...)` that
  awaits internally hides a suspend/cancellation point. Rejected for the
  visibly-awaited `async {}` expansion.
- **All-or-nothing fan-out / return-the-entry on cancel.** See operating rules 4–5;
  both fight the per-slot-independence the architecture is built on.
- **`block_on(send())` inside sync `log()` for async callers.** Spins an executor
  per call and exposes the caller's runtime to interactions (ADR 0002 rejected the
  sibling form). The honest fix is a real async entry point.

## Consequences

- **New opt-in dependency** — `event-listener`, behind the `async-log` feature. The
  default build is unchanged and the mailbox stays zero-dependency (ADR 0002
  preserved).
- **New public surface** — `Logger::log_async`, `global::log_async`,
  `info_async!`…`trace_async!`. The sync surface is untouched; no breaking change.
- **New metric** — per-transport `cancelled_total`, beside `dispatched_total` and
  `dropped_total`.
- **The niche served is narrow** — async runtime *and* must-not-drop *and*
  can't-park-a-worker. Every other case stays on sync `log()`: Drop policies for
  non-blocking, Block for a sync app's lossless lane. Guidance is to confine the
  `.await` to the must-not-drop call sites.
- **The half-built `send`/`AtomicWaker` path is replaced, not merely deleted** —
  resolving the latent multi-producer lost-wakeup the investigation flagged.

## Rollout (phased)

Each phase is independently shippable and its own commit set (lib / tests /
examples split per repo discipline). The risky concurrency work is isolated first.

- **Phase 0 — the waiter primitive.** `event-listener`-backed `send()`, feature-gated.
  No public API. Tested by a multi-producer async-send case that fails on the
  current single-slot waker and passes after; also fixes the latent lost-wakeup on
  its own.
- **Phase 1 — `Logger::log_async`.** Phase-1 `try_push` all slots, phase-2 `join`
  the `Block`-full awaits. Tested that it yields the task rather than parking.
- **Phase 2 — the cancellation contract.** `cancelled_total` + drop-guard, contract
  written onto `log_async`'s docs. Tested by cancelling a backpressured `log_async`
  and asserting the counter ticks and the entry did not land.
- **Phase 3 — global + macros.** `global::log_async`; `create_async_level_macros!`.
- **Phase 4 (optional) — polish.** Trait methods, an async-handler example on the
  must-not-drop lane, doc cross-links.

## References

- `docs/mailbox-concurrency-investigation.md` — the reasoning trail: the
  impossibility triangle, Route 1 vs the dispatcher, the half-built primitive.
- `docs/backpressure-prior-art.md` — the ecosystem survey (no mainstream logger
  offers cooperative async backpressure).
- `winston/src/mailbox.rs` — `send()` and the waiter primitive.
- `winston/src/pipeline.rs` — `dispatch_entry` (the sync twin `log_async` mirrors),
  `try_push_to_slot`, per-transport stats.
- `winston/src/logger.rs`, `winston/src/global.rs`, `winston/src/log_macros.rs` —
  the async surface.
- [ADR 0002](0002-direct-dispatch-backpressure.md) — direct-dispatch backpressure;
  the sync path this extends, and where `async fn log` was rejected (narrowly).
- [ADR 0003](0003-parallel-deferred-block-dispatch.md) — parallel deferred block
  dispatch; the phase-2 `join`.
- `event-listener` — <https://docs.rs/event-listener>.
</content>
