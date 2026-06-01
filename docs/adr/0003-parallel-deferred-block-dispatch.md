# ADR 0003 — Parallel deferred-Block dispatch

**Status:** Rejected (the gap it set out to close turned out to be illusory).

## Why rejected — the analysis error

The framing in the original Context below assumed serial phase 2 would
cause the caller to wait `sum(slot_drain_times)`. On closer trace this
isn't true: **the per-slot pumps run independently and are not gated by
phase 2's iteration order**. Each pump is an autonomous task that has
been processing its current entry since long before phase 2 started.

Walking through it with A draining at `t_A = 100ms` and B at
`t_B = 150ms`:

- **Serial**: worker `push_blocking`s A at T=0, parks. A's pump frees
  room at T=100. Worker wakes, moves to B's `push_blocking`. *B's pump
  has been working since T=0, finishes at T=150.* Worker reaches B at
  T=100, parks 50ms more, wakes at T=150. **Total = 150ms = max.**
- **Parallel**: both futures awaited concurrently. `join_all` completes
  at `max(t_A, t_B) = 150ms`. **Total = 150ms.**

Equivalent. The serial chain does not compound because B's pump isn't
waiting for A's pump — it's running in parallel anyway. The serial
visit just *observes* each pump's drain in turn; the slowest one bounds
the total because the slowest is what the worker is still waiting on
when faster ones have long since finished.

The proposal below (add `block_on(join_all(...))` to phase 2 for N > 1)
would have added executor cost on the saturated path with zero
performance benefit. We are keeping the existing serial loop.

This ADR stays in the tree as the record of why we considered the
optimization and why it doesn't apply, so the same intuition doesn't
get re-raised in six months.

## Context (the originally assumed gap)

ADR 0002 commits to "slowest pipe sets the pace" — the calling thread waits
for the slowest `OverflowPolicy::Block` slot whose mailbox is full, and that
wait is what bounds the producer's rate. The two-phase dispatch in the
direct-dispatch refactor (`refactor(logger): direct dispatch per ADR 0002`)
implements that with:

- **Phase 1** — non-blocking `try_push` to every eligible slot. Drop policies
  resolve here. Block slots with room resolve here. Only `Block` slots whose
  mailbox is full are deferred to phase 2.
- **Phase 2** — for each deferred Block slot, `push_blocking` parks the
  calling thread on that slot's `Condvar` until the slot has room.

Phase 2 iterates the deferred list **serially**: `push_blocking` on slot A
→ wait → `push_blocking` on slot B → wait. **The original framing of this
ADR claimed** the caller therefore waits `sum(drain_times)`, citing a
"100ms plus 150ms equals 250ms" example. That framing is wrong — see
*Why rejected* above. What follows preserves the proposal that was
considered on the basis of the flawed framing.

## Proposal (not implemented — see *Why rejected*)

When phase 2 has more than one deferred Block slot, await the per-slot
sends concurrently from the calling thread using
`futures::executor::block_on(futures::future::join_all(...))` over the
mailbox's async `send` future. When phase 2 has zero or one deferred
slot, take the existing zero-cost or `push_blocking` path.

Sketch:

```text
phase_2(deferred):
  if deferred.is_empty():
    return                                  // common path: no work
  if deferred.len() == 1:
    sync push_blocking on the single slot   // zero alloc, no executor
  else:
    block_on(join_all(async sends, one per slot))
```

The single-slot fast path matters because the common case is exactly
"one Block slot saturated," and there `push_blocking` is one Condvar wait
with no allocation — strictly cheaper than spinning up `block_on(join_all
([fut]))`.

## Why `block_on(join_all(...))`

`futures::executor::block_on` is not a runtime — it is a thread-local
`Parker` (Condvar + Mutex underneath) plus a poll loop. For N awaiting
futures it adds: one `Vec` allocation for the pinned futures, N polls per
wake, one Parker park. **Microseconds.** The Condvar wait inside it is
exactly the wait we'd be doing anyway; the dispatch primitive's overhead
is rounding error against the sink-drain time it's waiting for.

This is what makes it the right tool here: the *added* cost of the parallel
machinery only materialises when phase 2 has work, and even then it sits
inside an unavoidable sink wait.

## Operating rules

1. **Phase 1 unchanged.** Every slot with room (any policy) and every Drop
   slot still resolves in phase 1 — no allocation, no executor, no waiting.
   The fast path of `log()` does not touch `block_on`.

2. **Phase 2 of one slot is sync.** A single deferred Block slot bypasses
   `block_on` entirely; `push_blocking` is the cheapest available
   primitive for that case.

3. **Phase 2 of N > 1 slots is `block_on(join_all)`.** The calling thread
   parks on a single Parker; each mailbox `send` future progresses
   independently as its pump frees room. The calling thread unparks when
   the slowest one completes.

4. **No async runtime is introduced.** `futures::executor::block_on` is
   not tokio/async-std; it is std-only primitives in a poll loop. No
   threadpool, no work-stealing, no executor pinning. The mailbox `send`
   future is a small `Future` over the existing `MailboxSender` we
   already have.

5. **Drop policies are the architectural opt-out.** Phase 2 only ever runs
   for `Block`-policy slots. The way to keep a transport off the wait
   path entirely is to configure it `DropNewest` / `DropOldest` — those
   resolve in phase 1 and never touch the parallel-wait machinery. The
   right answer to "I don't want this transport to ever block my caller"
   is its `OverflowPolicy`, not a separate global escape hatch.

## Alternatives considered and rejected

- **Keep phase 2 serial.** Cheapest implementation, but compounds the
  wait. Violates the "slowest sets the pace" contract from ADR 0002 in
  the only configuration where the gap is observable.

- **Hand-roll a parking barrier with `std::thread::spawn` per deferred
  slot.** Drops the `block_on` dependency. But thread creation is ~100µs
  per slot per `log()` call — orders of magnitude more than the entire
  cost of `block_on` for the same shape. Strictly worse.

- **Persistent per-slot dispatch worker threads.** A thread per slot,
  long-lived, draining a per-slot "dispatch request" channel. `log()`
  pushes one request per Block slot then waits on a completion. No
  per-call thread spawn cost. The trade is N additional persistent
  threads per Logger; for low N (~5) reasonable, for many slots
  wasteful. Adds a whole new task model parallel to the existing pump
  tasks. Reasonable to revisit *if* benchmarks ever show `block_on` is
  the bottleneck (they will not, for any realistic config).

- **Mailbox `push_blocking_many(&[(slot, msg)])` primitive that takes
  multiple slots atomically.** Would let the mailbox handle the
  multi-slot wait internally with a single Parker. Folds the dispatch
  concern into the queue primitive — wrong layer; the mailbox is single-
  producer / single-consumer / single-queue and shouldn't grow cross-slot
  awareness.

- **Tokio-style runtime (per Logger or global).** Massive overkill for
  what is fundamentally "wait for N independent events." A runtime
  doesn't make Condvar waiting cheaper; it makes it work-stealable,
  which is the wrong axis for "park the calling thread."

- **`ArcSwap` + lock-free waiter coordination.** Same parking primitive
  ultimately needed; the lock-free veneer doesn't change anything for a
  parker-bound wait. The Parker is the floor; everything else is window
  dressing.

## What we would have got — and why we didn't take it

If the framing had been correct, the proposal would have given:

- **Tail latency improvement under multi-Block-saturation** — caller
  waiting `max(slot_drain_times)` instead of `sum(slot_drain_times)`.
  As traced in *Why rejected*, the serial form already gives `max`
  because the pumps run in parallel regardless of phase 2 ordering, so
  there is no improvement to be had.
- **No fast-path regression.** Logs that don't trigger phase 2 still
  pay nothing — but since we're not changing phase 2, this point is
  moot.
- **No new dependency, no new threads.** Same — moot for a non-change.

What we would have *paid* for nothing in return:

- One `Vec` allocation + N future pinnings per `log()` call that hits
  phase 2 with N > 1 — microseconds, but non-zero, on the saturated path
  that's already paying the actual sink wait. Adding executor cost
  without a corresponding semantic improvement is strictly worse than
  the existing loop.

The serial loop is kept. The proposal's analysis is preserved here
because the same intuition (sum vs max) is the kind of thing that gets
re-raised; recording the trace prevents a re-implementation attempt.

## References

- ADR 0002 — Direct-dispatch backpressure (this ADR was originally
  framed as closing a multi-Block semantic gap in 0002's two-phase
  dispatch; on analysis the gap doesn't exist).
- `winston/src/mailbox.rs::MailboxSender::push_blocking` — the
  primitive the serial loop already uses, which gives `max(drain_times)`
  for free because pumps are independent.
- `winston/src/pipeline.rs::StateSnapshot::dispatch_entry` — the
  two-phase dispatch site that stays serial in phase 2.
