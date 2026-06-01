# ADR 0003 — Parallel deferred-Block dispatch

**Status:** Accepted

## Context

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

Phase 2 currently iterates the deferred list **serially**: `push_blocking`
on slot A → wait → `push_blocking` on slot B → wait. If A takes 100ms to
free a spot and B takes 150ms, the caller waits **250ms**, not `max(100,
150) = 150ms`. Under "slowest sets the pace" the contract should give the
caller `max`, not the sum.

This is a real but bounded gap:

- It only shows up when **two or more** slots are simultaneously `Block`-policy
  AND simultaneously saturated.
- The common config has at most one Block slot (e.g. file = Block; http +
  console = DropNewest), so the existing serial form is already O(slowest)
  in practice.
- The case it does affect — multiple durable lanes (file + daily-rotate +
  remote-mirror) all backed up at once — is the precise scenario where
  latency matters most for the calling thread.

## Decision

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

## Consequences

- **Tail latency improves under multi-Block-saturation** — caller waits
  `max(slot_drain_times)` instead of `sum(slot_drain_times)` when phase 2
  has 2+ deferred entries.
- **Fast path unchanged.** Logs that don't trigger phase 2 (the dominant
  case) pay exactly nothing for this. The branch on
  `deferred.is_empty()` keeps the hot path free of any executor cost.
- **Single-slot phase 2 unchanged.** Single-deferred case still uses the
  zero-alloc `push_blocking` path — `block_on` only enters when N > 1.
- **No new dependency.** `futures::executor::block_on` and
  `futures::future::join_all` are already in the crate's dependency tree.
- **No new threads.** The calling thread parks on its own Parker; the
  per-slot pumps are the same async tasks already running. The mailbox
  `send` future borrows the producer (already supported in mailbox.rs).
- **The semantic gap in ADR 0002 closes.** "Slowest pipe sets the pace"
  becomes true for any N, not just N ≤ 1.

- **Cost scales with saturation frequency, not with throughput, slot
  count, or concurrency.** Concurrent `log()` callers don't contend
  (each has its own thread-local Parker). Slots per Logger contribute
  O(N) to the polling cost in phase 2, which for realistic N (≤ 5) is
  in the tens of nanoseconds. The only thing that meaningfully scales
  the parallel-wait cost is *how often phase 2 fires* — which is
  bounded by how often Block slots saturate. Healthy sinks → never
  enters phase 2 → zero cost from this machinery.

- **The primitive only spends cycles when there is nothing better to do.**
  Phase 2 fires when the caller is going to wait on a slow sink anyway;
  `block_on(join_all)` adds microseconds of polling on top of a wait
  measured in milliseconds. This is what makes it the right choice for a
  logger: efficiency on the hot path comes from not running it at all,
  not from making it incrementally cheaper.

## References

- ADR 0002 — Direct-dispatch backpressure (this ADR closes the
  multi-Block semantic gap left as a TODO at the bottom of 0002's
  dispatch description).
- `winston/src/mailbox.rs` — `MailboxSender::send` is the async send
  future this commit's phase 2 awaits.
- `winston/src/pipeline.rs::StateSnapshot::dispatch_entry` — the two-
  phase dispatch site that gains the N > 1 branch.
