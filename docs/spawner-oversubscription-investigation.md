# Investigation — multi-transport fan-out cost is thread oversubscription

**Status:** Investigation log / fixed by `pooled_spawner` (relates to
[ADR 0004](adr/0004-two-tasks-per-transport.md))

## How to read this

The render-dedup investigation (`docs/render-dedup-investigation.md`) found that
per-log cost scales **super-linearly** with transport count even with rendering
removed — so the dominant multi-transport cost is the fan-out, not the format.
This page runs that down to its cause (thread oversubscription), and the fix
(`pooled_spawner`).

Figures are from one machine; the *shape* (how cost scales with transport count K)
is the durable part.

## The clue

With render removed (passthrough), fan-out cost still explodes with K, and the
jump is concentrated between K=4 and K=8:

```text
passthrough fan-out:  K=1 1.04ms   K=2 1.40ms   K=4 2.18ms   K=8 12.7ms
                            ~linear ──────────────┘    └─ 5.8× for 2× transports
```

## The hypothesis: thread oversubscription

`default_spawner` runs **one OS thread per task**, and each transport runs **two**
tasks — a pump and a WS controller (ADR 0004). So thread count is `2K`:

```text
                     K=1    K=2    K=4    K=8     K=16
   threads (2K):      2      4      8     16       32
                                    ^ ≈ cores      ^ 2–4× cores → scheduler thrash
```

The blow-up at K=4→8 is exactly where `2K` crosses a typical core count. So the
cost is not the fan-out itself; it is oversubscribing the scheduler with idle-but-
wakeable threads.

## The decisive measurement

The `single_threaded_spawner` runs *all* tasks on one cooperative thread — no
oversubscription, but serialized. Running the passthrough fan-out under both
spawners (and later `pooled_spawner`) discriminates oversubscription from an
inherent fan-out cost: `winston/benches/logger_benchmark.rs`, group
`fanout_spawner`.

| K | `default` (2 threads/transport) | `single` (1 thread) | `pooled(4)` (4 threads) |
| --- | --- | --- | --- |
| 1 | 1.0 ms | 0.52 ms | 1.1 ms |
| 2 | 1.4 ms | 0.79 ms | — |
| 4 | 2.1 ms | 1.45 ms | — |
| 8 | 14.7 ms | 2.9 ms | 3.8 ms |
| 16 | 41.6 ms | 5.6 ms | 6.3 ms |

Scaling relative to K=1:

```text
   default   1.0ms ─────────────────────────▶ 41.6ms    40×   super-linear
   single    0.5ms ──▶ 5.6ms                            11×   linear (fastest absolute)
   pooled(4) 1.1ms ──▶ 6.3ms                            5.7×  linear, bounded threads
```

**Confirmed: thread oversubscription.** With thread count bounded (one, or a pool
of four), the fan-out scales **linearly**; only the unbounded thread-per-task path
blows up. Two corollaries:

- **`single` is faster at every K, including K=1** (0.52 vs 1.0 ms), because a
  transport's pump and controller share the one thread, so their handoff is
  *same-thread* — it removes the cross-thread wakeup the queue-depth investigation
  spent effort amortizing. `pooled` does not get this (round-robin may split a
  transport's two tasks across workers), which is why `pooled(4)` at K=1 matches
  `default`, not `single`.
- **The lever is the spawner, not the core.** winston is runtime-agnostic; nothing
  in the dispatch model is at fault.

## The fix — `pooled_spawner(n)`

A bounded pool of `n` cooperative executor threads (each the `single_threaded`
loop), with spawned tasks distributed round-robin. It caps threads at `n`
regardless of transport count, so it does not oversubscribe — and a transport that
blocks in `poll` stalls only the tasks sharing its worker, not all of them. It is
`~40` lines reusing the existing single-thread executor, needing no new dependency.

The three spawners now form a clear ladder:

| spawner | threads | scaling with K | a sink that blocks in `poll` |
| --- | --- | --- | --- |
| `default_spawner` | 2 per transport | super-linear (oversubscribes past ~4) | isolated — stalls nothing else |
| `single_threaded_spawner` | 1 total | linear, fastest | stalls **every** task |
| `pooled_spawner(n)` | `n` total | linear | stalls only its worker's tasks |

**Guidance:**

- **Few transports, or unknown/blocking sinks** → `default_spawner` (the default).
  Fine up to a handful of transports; per-task isolation is safest.
- **Many transports** → `pooled_spawner(≈ core count)` — scales linearly, tolerates
  blocking. The scalable middle ground.
- **Many transports, all fully non-blocking** → `single_threaded_spawner` — the
  fastest, but one blocking sink stalls everything.

The default is left unchanged: thread-per-task is a reasonable, isolating default
for the common few-transport case, and `pooled_spawner` is a one-line opt-in
(`Logger::new_with_spawner(opts, pooled_spawner(n))`) for the rest.

## References

- `winston/src/pipeline.rs` — `default_spawner`, `single_threaded_spawner`,
  `pooled_spawner`, `spawn_cooperative_executor`.
- `winston/benches/logger_benchmark.rs` — the `fanout_spawner` group.
- ADR 0004 — Two tasks per transport (the pump + WS controller split that makes
  thread count `2K` under `default_spawner`).
- `docs/render-dedup-investigation.md` — the sibling investigation that surfaced
  this thread.
