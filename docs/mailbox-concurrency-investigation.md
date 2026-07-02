# Investigation — the mailbox is SPSC by label, MPSC by use

**Status:** Investigation log / open decision on the async producer path (relates
to [ADR 0002](adr/0002-direct-dispatch-backpressure.md), which established the
per-slot mailbox and rejected an async `log()` API).

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

## Open questions / to extend

- **Which fork.** The trigger for anything past (A) is a concrete async
  must-not-drop caller appearing.
- **If (C): per-call await or a single dispatcher?** A per-call `log().await`
  (fork-1 ergonomics) is a waker-queue swap in the mailbox. A single dispatcher
  task — callers `try_push` non-blocking into a front queue, one owner task does
  the `send().await`s — keeps the call site non-blocking and needs no primitive
  change, but re-serialises dispatch and must re-derive the parallel fan-out of
  [ADR 0003](adr/0003-parallel-deferred-block-dispatch.md) inside the dispatcher.
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
