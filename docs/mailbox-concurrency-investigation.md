# Investigation — the mailbox is SPSC by label, MPSC by use

**Status:** Investigation log / reasoning trail. The async-producer decision
(fork C — build the opt-in async door via Route 1) is recorded in
[ADR 0009](adr/0009-async-logging-door.md); this page remains the exploration
behind it. Relates to
[ADR 0002](adr/0002-direct-dispatch-backpressure.md), which established the
per-slot mailbox and the sync `log()` this extends.

## How to read this

The per-slot mailbox (`winston/src/mailbox.rs`) documents itself as
"single-producer / single-consumer." The consumer half of that is true. The
producer half is not: `Logger::log(&self)` is called concurrently from many
threads (and the global logger is `&'static`), and every caller funnels through
the *same* `MailboxSender` — so the producer side is genuinely multi-producer.

A full mailbox can make a producer wait in one of three ways. Two of them handle
the multi-producer reality correctly; this doc records *why* (settled — no action
needed). The third — the async `send().await` path — does not, and is currently
dead code. This doc lays out what that path is for, why it would break if wired
up as-is, and the options for it (open — a decision to make).

Line references are to the code at the time of writing; the *reasoning* is the
durable part.

## Background — one mailbox, three producer wakeup paths

Each transport slot owns one mailbox: a bounded `VecDeque` behind a
`parking_lot::Mutex`, plus three wakeup primitives
(`winston/src/mailbox.rs:29-39`):

```text
consumer_waker : AtomicWaker   → wakes the single pump (async consumer)
producer_waker : AtomicWaker   → wakes an async producer parked in send().await
producer_condvar: Condvar      → parks an OS thread in push_blocking()
```

A producer that finds the mailbox full resolves it by one of three routes,
selected by the slot's `OverflowPolicy` (`winston/src/logger_options.rs:176`,
default `Block`) and by whether the caller is sync or async:

| path | on full | blocks what | wired into dispatch? |
| --- | --- | --- | --- |
| `try_push` / `force_push_dropping_oldest` | drop / evict-oldest | nothing (non-blocking) | yes — Drop policies |
| `push_blocking` | parks the calling **OS thread** on the condvar | the caller's thread | yes — Block policy, phase 2 |
| `send().await` | parks the **task**, waker in `producer_waker` | the caller's task | **no — no production caller** |

The hot path never touches a wait primitive: routing is lock-free
([ADR 0006](adr/0006-lock-free-routing.md)) and the common case is a
non-blocking `try_push` (`winston/src/pipeline.rs:571-606`). A producer only
waits at all when a `Block`-policy slot's mailbox is *already full* — i.e. the
sink is the bottleneck.

## The label vs the reality

The single-consumer claim is honest: there is exactly one pump per mailbox
(`poll_next` takes `&mut self`, `winston/src/mailbox.rs:193-221`), and only the
pump holds the receiver. `consumer_waker` is a single-slot `AtomicWaker` and that
is correct *by construction* — one consumer can never register two wakers.

The single-producer claim is a misnomer. `MailboxSender` methods take `&self` and
serialise concurrent callers on the internal mutex; the type deliberately isn't
`Clone` (`winston/src/mailbox.rs:80-83`). That is what makes many-producer use
*sound* — but it is many-producer, not single.

```text
                consumer          producer (sync path)     producer (async path)
 label:         single            single                   single
 reality:       single  ✓         MULTI (many log()s)       would be MULTI
 primitive:     AtomicWaker       Mutex + Condvar           AtomicWaker
 MP-safe?       yes (truly SP)    yes (condvar = waitlist)  NO (single slot)
```

The two producer paths that are wired in are MP-safe. The one that is dead is
not. The rest of this doc is those three cells.

## Settled — the condvar path (`push_blocking`) is correct under many producers

`poll_next` calls `producer_condvar.notify_one()` after each pop
(`winston/src/mailbox.rs:201`). `notify_one` here is the textbook-correct choice,
not a CPU-vs-latency compromise:

- Each `pop_front` frees **exactly one** slot. `notify_one` wakes one parked
  producer to fill that one slot — a 1:1 match between slots freed and producers
  woken. No waste.
- `notify_all` would be a thundering herd: one freed slot wakes all N parked
  producers, one wins the mutex and fills the single slot, and the other N-1
  re-evaluate `wait_while`'s predicate (`q.len() >= capacity`,
  `winston/src/mailbox.rs:123-125`), find it still true, and re-park. `notify_all`
  is the re-park *amplifier*, not the avoider.

`notify_one` is *safe* (not merely fast) because every parked producer waits on
the identical predicate — "the queue has room" — and any one of them can make
progress once a slot frees. That is precisely the precondition under which
`notify_one` cannot lose a wakeup.

### The load-bearing invariant (worth a code comment)

`notify_one()` fires *after* `drop(q)` — i.e. **outside** the queue lock
(`winston/src/mailbox.rs:199-201`). That is normally where lost-wakeup bugs live.
It is safe here only because `push_blocking` uses `wait_while` with a predicate
re-check *under* the lock: a producer that acquires the lock after the pop but
before the notify sees room and returns from `wait_while` without ever parking,
so the "missed" signal is irrelevant. This coupling — *notify outside the lock is
fine because the waiter re-checks the predicate under the lock* — is the real
correctness property of this path and is currently implicit. **Follow-up:**
promote it to a one-line comment at `mailbox.rs:201`.

### Why the condvar is structurally MP-safe

A `Condvar` keeps an internal waitlist of every parked waiter. That is the whole
difference from the async path below:

```text
Condvar waitlist:  [ T1, T2, T3, T4 ]     ← all remembered
   notify_one → wake T1, list [T2,T3,T4]
   notify_one → wake T2, list [T3,T4]      ← everyone eventually served
```

## Settled — the backpressure event tolerates the producer race by design

Two layers ride on the same dispatch. Only one is exact.

- **Layer A — delivery.** Every `try_push` / `push_blocking` takes the queue
  mutex (`winston/src/mailbox.rs:88`, `:122`). Messages are never lost,
  duplicated, or reordered within a slot regardless of producer count. This is
  what makes the MPSC use sound.
- **Layer B — the `BackpressureEvent` edge.** The `was_saturated` swap plus
  `emit_event` runs *without* the queue lock (`winston/src/pipeline.rs:575`,
  `:584`). This is deliberately approximate, and the slot doc says so
  (`winston/src/pipeline.rs:303-307`).

`BackpressureEvent` is **edge-triggered** — it fires only on the empty↔full
transition, not per entry — using the previous value returned by the swap:

```text
swap(true)  returns false  →  this caller flipped it, owns the edge  →  emit Saturated
swap(true)  returns true   →  someone already flipped it             →  stay silent
swap(false) returns true   →  this caller cleared it                 →  emit Recovered
```

Because the swap+emit is not atomic with the push/pop, concurrent producers can
emit a **bounded duplicate**. With capacity 1:

```text
was_saturated=false, queue=[x] (full)

T1: try_push -> FULL          (both observe full,
T2: try_push -> FULL           neither has swapped yet)

T1: swap(true)->false  ───────────────────────►  emit Saturated (A)   was_saturated=true
        -- consumer pops -->  queue=[]
T3: try_push OK -> [x]  swap(false)->true ─────►  emit Recovered       was_saturated=false
T2: swap(true)->false  ───────────────────────►  emit Saturated (B)   was_saturated=true

Emitted: Saturated(A), Recovered, Saturated(B).
```

`Saturated(B)` is a redundant re-announcement, but it is not *wrong*: the queue
genuinely is full again at the end. The count of such stragglers is bounded by
how many producers were mid-`try_push` across the edge — i.e. by concurrent
producer count, never unbounded. Two guarantees survive the race:

1. **No transition is ever missed** — if any dispatch meets a full mailbox, at
   least one producer wins `swap(true)->false` and emits `Saturated`.
2. **No unbounded spam** — the bit still dedups the common case; only straddling
   producers leak an extra event.

This is correct by design: `BackpressureEvent` is an *advisory* signal (a
subscriber reacting to a duplicate `Saturated` just throttles once more than
necessary). Exact numbers are `TransportStats`' job —
`dispatched_total` / `dropped_total` use precise atomic `fetch_add`
(`winston/src/pipeline.rs:561`, `:593`, `:598-599`). The split is intentional:
precise counters via atomics, cheap advisory edges via the unlocked swap, so the
hot path takes no extra lock merely to *maybe* emit an event. Making it exact
would mean holding the queue mutex across `emit_event` (a channel send) on the
hot path — cost for no real benefit.

## Open — the async producer path (`send` / `producer_waker`) is half-built

The async `send().await` path exists in the mailbox but is not connected and is
not correct for the multi-producer reality. "Half-built" is a specific claim,
scored against what a finished path requires:

```text
                          API exists   wired to a    tested         correct under
                          & compiles   real caller                  the actual reality
                        ┌───────────┬─────────────┬──────────────┬──────────────────┐
 push_blocking (sync)   │    ✓      │     ✓        │     ✓        │   ✓  (condvar =   │
                        │           │ pipeline.rs  │ mailbox.rs   │      waitlist,    │
                        │           │   :560       │   :301       │      MP-safe)     │
                        ├───────────┼─────────────┼──────────────┼──────────────────┤
 send() (async)         │    ✓      │     ✗        │  ✓ but only  │   ✗  (AtomicWaker │
                        │ :137-191  │  NO caller   │  1 producer  │   = 1 slot,       │
                        │           │              │   :280-298   │   breaks under MP)│
                        └───────────┴─────────────┴──────────────┴──────────────────┘
```

**Present:** the `send()` method (`winston/src/mailbox.rs:137`), the full
`Future::poll` including a register-then-re-check race guard (`:156-191`), the
`producer_waker` field (`:33`), the consumer waking it on every pop and on drop
(`:200`, `:227`), and a passing unit test (`:280-298`).

**Absent:** no production caller — the pipeline's phase-2 backpressure calls
`push_blocking` (`winston/src/pipeline.rs:560`), and there is no `log().await`
entry point on the logger at all (`winston/src/logger.rs:285-321` are all sync).
And the one test uses a single producer (`:283-284`), so the defect below can
never fire in it.

### The defect — `AtomicWaker` is a single slot

`AtomicWaker` holds exactly one waker. The consumer side is fine (one pump). The
producer side is not: `Send::poll` registers into that one slot
(`winston/src/mailbox.rs:176`), so a second concurrent async producer clobbers
the first's registration.

```text
producer_waker: [ empty ]

task A:  send(..).await → full → register(Wa)     producer_waker:[Wa]
task B:  send(..).await → full → register(Wb)     producer_waker:[Wb]   ← Wa OVERWRITTEN

  -- consumer pops one slot --
  producer_waker.wake()  → wakes Wb, clears        producer_waker:[empty]

task B:  re-polled → room → pushes → Ok
task A:  never woken. Future stays Pending forever  ← LOST WAKEUP → hung task
```

The re-check at `winston/src/mailbox.rs:179-185` closes a *different* race (a pop
landing between the length check and the register); it cannot help here, because
A's waker was already clobbered at register time, so A is simply never re-polled.
The condvar path survives the identical scenario because it keeps a waitlist; the
single-slot `AtomicWaker` does not. Same asymmetry as the table above.

### Why anyone would want this path — task-park vs thread-park

The motivating scenario is a shared logger under an async runtime with a
`Block`-policy slow sink. `push_blocking` parks the calling **OS thread** via
`Condvar::wait`, and that has no yield point — from the runtime's view the future
enters `poll()` and never returns control:

```text
async task → logger.log(entry) → phase 2: push_blocking → Condvar::wait
   the future NEVER returns Poll::Pending
   the worker thread is frozen mid-poll, deep in a kernel park
   a cooperative scheduler cannot preempt what never yielded
```

- **Multi-threaded runtime:** one worker thread is lost for the duration. Enough
  concurrent blocked `log()`s → all workers parked → nothing left to run the pump
  that would drain the mailbox → the runtime wedges.
- **Single-threaded runtime:** immediate self-deadlock — the parked thread is the
  same one that would run the pump, so the park never lifts.

`send().await` returns `Poll::Pending` instead: it suspends the *task* (a small
heap struct) and frees the worker to run others; the waker re-schedules when room
appears. Moving the wait from thread-park to task-park is the entire point of the
path.

```text
push_blocking (today):   task ═══welded═══ thread   → kernel parks BOTH
send().await (the goal):  task ──suspended──          → Poll::Pending
                          thread ──runs other tasks──   waker reschedules on room
```

### Why the spawner cannot substitute for it

`spawn_fn` (`winston/src/logger_builder.rs:29`) governs the **consumer** side —
where the pump/writer tasks run (`winston/src/pipeline.rs:260`) and where query
streams run (`winston/src/logger.rs:368`); it is the BYO-runtime knob for the
drain (see `docs/spawner-oversubscription-investigation.md`). The block, however,
lands on the **producer** side: `log(&self)` runs synchronously on the *caller's*
thread and parks *that* thread in phase 2, before any spawner is involved.

```text
  caller's thread ──► log() ──► [phase 2] push_blocking ──► PARKS HERE
                                                            (no spawner in this path)
  spawn_fn only decides where THIS runs: ──► pump task ──► drains mailbox ──► sink
```

So there is an asymmetry: the drain's placement is configurable; the placement of
`log()`'s block is not — it is always the caller's thread. Only an async front
door (`send().await`) moves that particular wait from thread to task.

### The non-blocking policy matrix (dispelling the false dilemma)

"Do I have to write an ugly `log().await` to log without blocking a thread?" — no.
Non-blocking is the *default*; thread-parking lives in exactly one cell:

```text
                        mailbox has room          mailbox FULL
                        (≈ always, hot path)      (sink is the bottleneck)
                     ┌──────────────────────────┬──────────────────────────┐
 Block (default)     │  try_push → non-blocking  │  push_blocking → PARKS    │
                     │                           │  the thread ← only cell   │
                     ├──────────────────────────┼──────────────────────────┤
 DropNewest /        │  try_push → non-blocking  │  drop / force_push →      │
 DropOldest          │                           │  non-blocking             │
                     └──────────────────────────┴──────────────────────────┘
```

- Non-blocking under saturation → pick a Drop policy. This is the ecosystem-normal
  answer (`tracing`, `slog-async`), async-safe today with the plain sync `log()`.
- Lossless backpressure in a sync app → `Block`; parking a thread-per-request or
  pool thread is cheap and correct there.
- Lossless backpressure in async without parking a worker → the niche the async
  path serves. Bridgeable **today with no library change** via
  `spawn_blocking(move || logger.log(entry))`, which moves the park to the
  blocking pool (cost: a task hop and blocking-pool pressure under high
  concurrency; drops nothing; unwedges the single-threaded case too).

Across all three, `queue_capacity` (default 1024, per slot) governs *how often*
the full-cell is reached rather than *what happens* there: raising it lets the
mailbox absorb a larger burst before the policy fires, trading memory for fewer
saturation episodes. It and the per-slot `OverflowPolicy` are the two
**producer-side** backpressure levers; `spawn_fn` is the separate
**consumer-side** one (it places the drain, never the caller's block — see the
spawner asymmetry above).

## The decision space (forks)

Only the last row of the matrix — async **and** must-not-drop **and**
can't-park-a-worker — needs the async producer path. Three ways forward:

- **(A) Delete `send` / `Send` / `producer_waker`.** YAGNI: the pipeline never
  uses them, and a dead, MP-unsafe concurrency primitive that *looks* finished is
  a trap. Async apps use a Drop policy or `spawn_blocking`. Cheapest and honest.
- **(B) Keep but fence it.** Document `send` as single-async-producer-only and
  `debug_assert` a single registrant, so a second concurrent `register` fails
  loud instead of hanging silent. Interim if the path is kept but not yet needed.
- **(C) Make it MP-safe, then expose it.** Replace the single `AtomicWaker` with a
  waiter queue mirroring the condvar — `Mutex<VecDeque<Waker>>`, push a clone on
  `Pending`, pop-and-wake one per freed slot in `poll_next`. Waking one waiter per
  freed slot is the async twin of `notify_one` per pop — same 1:1 discipline, no
  thundering herd — and makes the mailbox honestly MPSC on both sides. Then add
  the async `log` entry point.

Current lean: **(A) now**, **(C) if/when an async dispatch path is actually
built**, **(B)** only as an interim if the path is kept in the meantime. Note that
[ADR 0002](adr/0002-direct-dispatch-backpressure.md) already rejected *replacing*
`log()` with `async fn log` (it would break every sync call site — the `log`-crate
adapter, `println!`-style usage). The niche here is *adding* an async variant
alongside the sync one, which 0002 does not foreclose.

## Building the async door (fork C in depth)

Fork C — exposing a cooperative async logging path — has enough structure that
its shape is worth recording before any code lands.

### What the await waits on — room, never the write

The sink is already detached from the caller: `log()` never waits for a write. The
pump owns that, draining the mailbox and doing
`writer.enqueue_when_ready(entry).await` on its own task
(`winston/src/pipeline.rs:329-332`). Even sync `Block` only waits for *a slot to
free* so the entry can be handed off — never for the entry to reach the sink.

So the async door's only await point is **enqueue room**. `send().await` returns
`Ready` on the first poll when the mailbox has space (no suspension at all); it
yields only when full, and resumes the instant the pump pops one entry and frees a
slot (`winston/src/mailbox.rs:169-173`, `:180-184`). It resumes on "a slot freed,"
not "my entry written" — identical semantics to `push_blocking`, task-yield
instead of thread-park. Under sustained overload, "room to enqueue" is paced by the
sink's drain rate, which is what makes it real backpressure — but it throttles the
caller to the sink's rate, never to any specific write.

Two await-able things must not be conflated:

| await… | meaning | API |
| --- | --- | --- |
| enqueue / room | wait until the buffer can accept the entry — backpressure | the async `log_async` |
| write / durability | wait until everything queued is written out | `flush()` |

`log()` — sync or async — always means "hand off to the buffer," never "wait until
durable." Await-until-written is `flush()`, a far stronger and more expensive
guarantee that has no place per-log-line.

### The impossibility triangle — the await cannot be designed away

Under sustained overload (entries arriving faster than the sink drains, long
enough to fill the buffer), the enqueue boundary offers exactly three moves, and
they trade off against three desirable properties — pick any two:

```text
                        lossless (Block)
                          /          \
              [Block-thread]        [Yield-task]
              sync call ✓           no thread-park ✓
              no park   ✗           lossless      ✓
                         \          / needs .await ✗
                    sync call ─── [Drop] ─── no thread-park
                             lossless ✗
```

- **Block the thread** (sync `Block`): lossless + sync call, but parks the worker.
- **Drop** (Drop policies): sync call + no park, but lossy.
- **Yield the task** (async door): lossless + no park, but needs `.await`.

The pigeonhole makes this fundamental, not incidental: infinite inflow, finite
outflow, finite memory, no blocking, no dropping is a contradiction. An
intermediary buffer adds a larger cushion before the boundary is hit; it does not
add a fourth corner. So the `.await` *is* the mechanism of cooperative lossless
backpressure — a yield point in the caller's control flow — not removable
ugliness. Delete it and saturation forces Drop or thread-park.

Every mature logger resolves this triangle the same way — by picking a corner, and
their "non-lossy" option is always thread-parking, never an async-cooperative
`.await`. The survey is in
[backpressure-prior-art.md](backpressure-prior-art.md).

### Two routes to the door

**Route 1 — direct per-slot `send().await` (needs step A).** `log_async` fans out
like `dispatch_entry`: phase 1 `try_push` to every slot, phase 2 `send().await`
the `Block`-full slots. Each slot stays independent — a caller awaits *per-slot*
room, no shared upstream queue, per-transport `OverflowPolicy` intact. The only
blocker is the single-slot `AtomicWaker`; step A's waiter queue removes it.

**Route 2 — single dispatcher task.** Callers `try_push` (non-blocking) into one
front queue; a single owner task drains it and does the `send().await`s. Its one
clean benefit: the dispatcher is the *sole* producer into each mailbox, so the
`AtomicWaker` is correct by construction — Route 2 sidesteps step A. Its sharp
cost is head-of-line blocking, the exact coupling
[ADR 0002](adr/0002-direct-dispatch-backpressure.md) removed:

```text
today (per-slot, independent):        dispatcher (shared front queue):

log() ─┬─► [file mailbox] ─► pump     log() ─► [ ONE queue ] ─► dispatcher ─┬─► file
       ├─► [console]      ─► pump                                            ├─► console
       └─► [http]         ─► pump                                            └─► http
   each drains independently          a slow Block sink parks the dispatcher →
                                       front queue backs up → every lane stalls
```

A slow `Block` sink stalls the dispatcher, backs up the shared queue, and blocks
console + http logging behind the file sink. A shared upstream queue also cannot
express per-transport policy at its own boundary — when full it makes one decision
for all lanes. The escape (per-transport front queues) just re-creates the
mailboxes with an extra hop. So Route 2 either collapses per-transport
independence or is redundant.

**Recommended: Route 1.** The waiter queue is ~15 local lines and preserves the
per-slot independence the architecture is built on; the dispatcher trades that
independence away to save it.

### ADR 0002, reconciled

[ADR 0002](adr/0002-direct-dispatch-backpressure.md) rejected an intermediary
queue for two reasons: *unbounded* (turns `Block` into silent-drop-at-OOM) and
*shared/layered* (a second pressure point that collapses the per-transport model).
A bounded per-path queue dodges the first; a shared upstream queue hits the
second. The objection was never the queue's existence or size — it is that a
*shared, upstream* buffer fights the per-transport architecture. Separately, 0002
rejected *replacing* `log()` with `async fn log`; *adding* an opt-in async variant
alongside the sync one is not foreclosed.

### API surface, build order, and the macro layer

The async door is opt-in and additive; sync `log()` is unchanged. The `.await`
belongs only to the must-not-drop lane — droppable logs (telemetry, debug, request
lines) stay on sync `log()` with a Drop policy (no await, no park). The ergonomic
cost shrinks to the few call sites that genuinely cannot drop.

Build order — the macros are the veneer, built last:

1. **Step A** — the mailbox waiter queue (`send` MP-correct).
2. **`Logger::log_async(&self, entry)`** — async twin of `dispatch_entry`
   (`winston/src/pipeline.rs:557-563`); phase 2 awaits the `Block`-full slots.
3. **`global::log_async(entry).await`** — mirror of `global::log`
   (`winston/src/global.rs:51`); `is_level_enabled` stays sync.
4. **Async macros** — a `create_async_level_macros!` mirror of
   `create_level_macros!` (`winston/src/log_macros.rs:79`). The only delta per arm
   is `.log(entry)` → `.log_async(entry).await`.

The macro should expand to an `async {}` block so the suspend point is **visible**
at the call site (`info_async!(...).await`), not hidden inside a statement — an
awaited log is a cancellation/ordering point and async Rust reads it as one. The
level-gate and entry construction live inside the block, so laziness is preserved
(nothing built when the level is disabled).

### Two edges to handle

- **Cancellation drops.** `info_async!(...).await` is a cancellation point: a task
  dropped while the `send` is `Pending` (mailbox full) loses that entry — never
  enqueued. The async lane's guarantee is *lossless under backpressure, not under
  cancellation*. This surprises exactly the caller who reached for it to avoid
  dropping; document it loudly on `log_async`.
- **Parallel phase 2.** Awaiting the `Block`-full slots serially lets one slow slot
  delay the rest. `join_all` / `FuturesUnordered` over the per-slot `send().await`s
  keeps them waiting in parallel — the async incarnation of
  [ADR 0003](adr/0003-parallel-deferred-block-dispatch.md).

## Open questions / to extend

- **Fork chosen: C**, recorded in [ADR 0009](adr/0009-async-logging-door.md) —
  build the opt-in async door via Route 1 (direct per-slot await on an
  `event-listener` waiter), `_async` naming, phased rollout. This section is the
  reasoning behind that choice.
- **Fork C's shape is worked out** under *Building the async door* — Route 1
  (direct per-slot await) over Route 2 (dispatcher). What stays open there: the
  go/no-go, the API naming (`log_async` / `info_async!`), and the exact
  cancellation contract to publish on `log_async`.
- **Promote invariants to code comments.** The notify-outside-the-lock safety
  property (`mailbox.rs:201`) and the SPSC-label caveat on the producer side
  (`mailbox.rs:21`) are load-bearing and currently implicit.

## References

- `winston/src/mailbox.rs` — the mailbox and its three wakeup paths; the async
  `send`/`producer_waker` path (`:137-191`).
- `winston/src/pipeline.rs` — two-phase dispatch (`:557-563`), `try_push_to_slot`
  and the `was_saturated` event race (`:571-606`), the consumer-side spawn site
  (`:260`).
- `winston/src/logger.rs` — `Logger::log` and its sync siblings (`:285-321`); the
  slowest-`Block`-slot rate note (`:89`).
- `winston/src/logger_options.rs` — `OverflowPolicy` and its `Block` default
  (`:176`, `:184`).
- [ADR 0002](adr/0002-direct-dispatch-backpressure.md) — direct-dispatch
  backpressure; introduced `push_blocking` + per-slot `OverflowPolicy`, rejected
  an async `log()` API.
- [ADR 0003](adr/0003-parallel-deferred-block-dispatch.md) — the parallel deferred
  block dispatch a dispatcher-based fork would have to preserve.
- [ADR 0006](adr/0006-lock-free-routing.md) — the lock-free hot path that keeps
  all of the above off the common case.
- `docs/spawner-oversubscription-investigation.md` — the spawner (consumer-side)
  knob and why it doesn't reach the producer-side block.
- [backpressure-prior-art.md](backpressure-prior-art.md) — how tracing-appender,
  slog-async, zap, and zerolog handle the same sync-call backpressure tension
  (they all pick a triangle corner; "lossless" always means block a thread).
