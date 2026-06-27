# Investigation — deriving the WritableStream queue depth (WS HWM)

**Status:** Investigation log / worked example (supports
[ADR 0006](adr/0006-lock-free-routing.md))

## How to read this

This is the record of how one tuning constant — the per-transport WritableStream
high-water mark (WS HWM) — went from a guess to a derived model, and of the three
benchmark artifacts that had to be cleared on the way. It is written as a worked
example because the *method* generalizes: each stage was forced by a pointed
question that exposed the previous answer as resting on an untested assumption or
a flawed measurement. Those questions are kept as the section drivers, because the
discipline they enforce — *don't trust a number you didn't measure; don't trust a
measurement you didn't design against artifacts* — is the transferable part.

A reader in a hurry can read the question headings alone and follow the argument.
The conclusion is in §12. The catalogue of traps is the appendix.

All figures are from a single machine; absolute throughput is machine-specific and
run-to-run noise is ~±5–6%. The *directions*, *ratios*, and *shapes* are the
durable results.

---

## 0. The system under test

Per transport, a log entry crosses **two bounded queues in series**, moved by
three workers on three threads:

```text
                  mailbox (cap N)           WS queue (HWM H)
   caller ─push─▶ ┌────────────┐ ─pump──▶  ┌────────────┐ ─controller─▶  sink
                  └────────────┘           └────────────┘
                   policy buffer            pump↔controller throttle
                   (Block / Drop here)      (ready() gate here)
```

- **mailbox** (`cap = queue_capacity`) — the synchronous front door. It bears the
  user's `OverflowPolicy`: `Block` parks the calling thread; `Drop` discards. This
  is the buffer the user configures and reasons about.
- **WS queue** (`HWM`) — the WritableStream's internal queue. It carries **no
  policy**; it is purely the throttle between the pump task and the controller
  task.

The backpressure mechanism, taken from the `whatwg_streams` source rather than
assumed (a confirmation step that matters, because the whole "is the WS HWM a
user-facing knob?" question turns on it):

- `writer.write(chunk)` does an `unbounded_send` of a `Write` command into the
  stream task's channel — **immediate, non-blocking** — and returns a future that
  resolves on sink completion.
- The pump calls `enqueue_when_ready`, which is `ready().await?` then
  `let _ = self.write(chunk)` — it **drops** the completion future, so it never
  awaits the sink.
- Backpressure is the `ready()` / `desired_size` gate alone:
  `desired_size = HWM − queue_total_size`, and the controller decrements
  `queue_total_size` and recomputes backpressure **only after `sink.write().await`
  completes** — one in-flight write at a time. The command channel itself is
  unbounded; the *only* thing bounding the WS queue is the cooperative `ready()`
  gate the pump honors.

The load-bearing consequence, established before any benchmark: there are **two
independent backpressure points**, and the WS HWM is *not* the user-facing one.
The mailbox is the policy buffer (it must equal `capacity`); the WS HWM is internal
flow control (a free variable). Everything downstream depends on keeping those two
roles distinct.

---

## 1. Q — "Why 64? Is there arithmetic, and would a different sink change it?"

The WS HWM had been set to `64` as part of a "shrink the second buffer" change.
The first and most important question is whether that number was *derived* or
*guessed*.

**Honest answer: guessed.** `64` was "a round number comfortably above the knee,"
validated only indirectly by a −24% result that was in fact dominated by an
unrelated change (lock-free routing), never isolated, and never measured above
`capacity`. It was a placeholder, not a computed value.

**Is there arithmetic?** A principle, but a *threshold to clear*, not a formula. The
working model at this stage was **handoff amortization**: the pump and controller
are separate threads, so each `write()` that crosses the channel costs a
cross-thread wakeup; with HWM=H the pump enqueues ~H entries, parks once, and is
woken once, giving

```text
per-entry wakeup overhead  ≈  wakeup_latency / H
```

A cross-thread wakeup is ~1–2 µs and per-entry work is ~1 µs, so the overhead is
~100% at H=2 and falls under ~5% by H≈16–32. `64` "sits safely past that knee."
This model predicts **deeper is monotonically better, then flat** — remember that;
it is about to be falsely confirmed.

Two consequences followed *if* that model were the whole story:

1. The HWM should be a **fixed small constant, not a function of capacity** — the
   amortization does not care how deep the mailbox is, so scaling HWM with capacity
   over-buffers for no benefit.
2. The exact constant is platform- and workload-dependent (wakeup latency,
   per-entry cost), so it must be **measured, not asserted**.

**Would a different sink change it?** Yes, in two senses worth separating:

- **Sink speed.** The HWM matters *most* for a fast sink and *least* for a slow
  one. A noop sink drains instantly, so the pump hits the throttle boundary every H
  entries — maximal sensitivity. A slow I/O sink drains at its own rate, the WS
  queue stays full, the pump parks and *stays* parked, and the wakeup rate is set
  by the sink regardless of H. So a noop benchmark is the **worst case** for HWM
  sensitivity — a value tuned there is conservative-safe for real sinks.
- **Sink granularity.** A one-entry-at-a-time sink (console) needs only
  handoff-amortization → a small constant. A *batching* sink (buffered file, an
  HTTP sink that POSTs N entries per request) would want a deeper queue so the
  controller can pull a batch. The current model writes one entry at a time, so the
  right HWM is sink-independent — but if batching sinks ever land, the HWM stops
  being a global constant and becomes a per-transport, sink-determined knob.

The honest position at the end of §1: `64` is a guess, the model behind it predicts
"deeper is better," and the only legitimate next move is to measure — not to ship a
number.

---

## 2. Q — "Could the capacity be shared half/half, or the WS made a derivative of capacity?"

Before measuring, a design question: should the user's `capacity` be *split*
between the two buffers (half mailbox, half WS), or the WS sized as some fraction
of capacity?

The structural answer comes straight from §0's two-backpressure-points result. The
WS HWM's only job is to amortize the internal handoff — an **absolute** need (a
handful to tens of entries), unrelated to how deep the *policy* buffer is. So:

- **Half/half** breaks at both ends: `capacity = 4` → WS = 2 (handoff overhead
  returns); `capacity = 1024` → WS = 512 (pure waste).
- **A capacity-derivative** has the same failure: tie an absolute need to a
  proportional knob and it is wrong at every scale except one.

So the design answer is "neither — the mailbox is `capacity` (policy), the WS is its
own thing." But this is still *reasoning*, not data. The §1 model and the §2
argument both point the same way ("WS is a fixed-ish absolute, decoupled"), which is
suggestive — and exactly the kind of agreement that should be *tested*, not trusted.

---

## 3. First measurement — and the conclusion it seemed to force

The first sweep reused the existing `logger_benchmark` `noop` group, which logs a
fixed **1000-entry burst then flushes**, sweeping the HWM against a fixed baseline:

| WS HWM | noop | multi/1 | multi/4 |
| --- | --- | --- | --- |
| 1024 | −18% | −22% | −1% |
| 256 | −16% | −20% | −2% |
| 64 | −11% | −14% | +5% |
| 16 | −8% | −9% | −3% |

Reading at the time: **bigger HWM monotonically better, plateauing near 1024.** This
appeared to confirm the handoff-amortization model *and* settle the design at
`WS HWM = capacity`. It was written up that way, and the only apparent cost — the
worst-case memory — was reasoned about as follows (kept here because the picture is
correct and is the right mental model for "what does buffer depth even cost"):

**Scenario A — the sink keeps up (≈ the normal case).** Every entry is grabbed by
the pump, then the controller, then written, immediately. Nothing piles up; both
queues sit at ~0–1 occupancy regardless of their *capacity*. The 1024 slots are a
ceiling, not an occupancy — memory in flight is a few KB.

**Scenario B — the sink can't keep up (sustained saturation, the rare case).**
Backpressure propagates backward: the controller is stuck in `sink.write().await`,
the WS queue fills to N, the pump's `ready()` gate parks, the mailbox fills to N,
and only then does the caller hit its `OverflowPolicy`. Both full ⇒ **N + N = 2N**
entries resident (~400 KB/transport at N=1024). This "2×" is a worst-case ceiling
reached only under sustained back-pressure, not typical occupancy.

So the §3 conclusion was: keep WS = capacity; the 2× is a rare-case ceiling and
shrinking it would (per the table) cost throughput on the common path. Plausible,
table-backed, and — as the next question shows — wrong.

---

## 4. Q — "Are we sure? Was 1024 actually *measured* as a plateau?"

The pointed question that broke §3: the table shows the curve flattening *toward*
1024, but was anything *past* 1024 tested, and is the flattening a property of the
queue or of the benchmark?

Two artifacts surface under that scrutiny, and together they void the §3
conclusion:

**Artifact A — the `.min(capacity)` clamp.** During the sweep the HWM was computed
as `WS_PIPELINE_HWM.min(capacity)`, with `capacity` defaulting to 1024. Every value
above 1024 was silently clamped *to* 1024. HWM > capacity was **never measured.**
"1024" was the ceiling the harness allowed, then mistaken for a plateau.

**Artifact B — the burst-size plateau.** The benchmark logs exactly 1000 entries
then flushes. The pump can never get more than ~1000 entries ahead of the
controller, so **once HWM ≥ ~1000 the pump never parks at all** and throughput tops
out; below that it ping-pongs ~`1000/HWM` times. The apparent "plateau" is pinned to
the **burst size**, not to any intrinsic property. Change the burst to 100 and the
knee moves to ~100; run it continuously and there is no hard plateau at all. The
knee tracked the *workload*.

So the first sweep established nothing about an intrinsic plateau: it measured "is
HWM ≥ burst," dressed as "is HWM ≥ 1024." The handoff-amortization model had not
been confirmed — it had merely escaped contradiction by a benchmark structurally
incapable of contradicting it. (Meta-lesson, banked for the appendix: a model that
*predicts* a flawed measurement is the most dangerous kind, because it stops you
looking.)

---

## 5. Q — "If a plateau is real and tested, shouldn't we cap the WS at it?"

A reasonable follow-through: if the WS throughput benefit truly plateaus while its
memory cost grows linearly, then for a very large `capacity` (say 100k) the WS depth
above the plateau is pure memory for no return, and `WS = min(capacity, X)` would
trim it. The reasoning is sound *conditionally*, and it forces two clarifications.

First, is the huge-capacity case "the same thing, just bigger"? On the **memory
axis**, yes — `2N` is one scale-invariant rule (1k→2k, 100k→200k), one thing the
user learns once. On the **throughput axis**, only if the plateau is real: the memory
cost grows linearly forever while the throughput benefit (supposedly) flattens, so
the two halves of the "2×" would not age the same. A cap is "stop allocating WS depth
that buys nothing," a no-op below the plateau.

Second — and decisive — the cap needs a number `X`, and §4 just established **there
is no measured `X`.** The plateau that would justify it was an artifact. So a cap now
would encode a guess as if it were data. Two conclusions:

- **No cap by default**, independent of the throughput question, because it trades a
  clean scale-invariant rule ("always 2N") for a piecewise one ("2N up to X, then
  N + X") to trim memory a user who set a huge capacity explicitly opted into.
- The cap is **gated on a real measurement**, not on taste — and the current
  benchmark cannot provide it (artifact B). The next step is therefore not a code
  change but a *better benchmark*.

---

## 6. Building an artifact-free benchmark

The design of `winston/benches/queue_depth.rs` is the crux; every choice exists to
kill a specific artifact:

- **Sustained, not burst.** Log `BATCH = 500_000` entries with `BATCH ≫ HWM`, so the
  queues live at steady-state depth instead of a short burst fitting inside the
  queue (kills artifact B).
- **`Block` policy.** Once the queues fill, the producer is throttled to the drain
  rate, so simply timing the `log()` calls measures the **drain rate** directly.
- **Cheap producer, near-noop sink.** A `passthrough` format (no render) keeps the
  producer faster than the consumer, so the *consumer chain* (pump → controller →
  sink) is the bottleneck — the only regime where the WS HWM can matter. The sink
  only counts writes, so the bottleneck is the queue machinery, not I/O.
- **WS decoupled from capacity.** A feature-gated `WINSTON_WS_HWM` env override
  (`--features internal-bench`) sets the WS depth independently of the mailbox — the
  only way to isolate the two, and the removal of the `.min(capacity)` clamp (kills
  artifact A).

A **third artifact appeared on first run of this very bench** and had to be removed:

**Artifact C — the flush tail.** The first version flushed each batch. But a flush
waits for the backlog to drain, and that backlog *scales with HWM* (a deeper WS holds
more un-drained entries at flush time). So flushing made deep-WS look slower for a
reason unrelated to steady-state throughput. Removing the per-batch flush — relying
on `Block` to throttle the producer to the drain rate — gave a clean measurement.
(That the artifact-killing benchmark itself sprouted an artifact is the whole point:
the discipline is continuous, not a one-time setup.)

---

## 7. The reversal — shallow WS is *better*

With all three artifacts cleared, the WS-isolated sweep (mailbox fixed at 1024, no
flush, sustained) said the **opposite** of §3:

| WS HWM | throughput |
| --- | --- |
| 2 | 1.07 M/s |
| 8 | 1.01 |
| 16 | 1.05 |
| 64 | 1.05 |
| 256 | 0.99 |
| 1024 | 0.92 |
| 4096 | 0.85 |
| 16384 | 0.82 |
| 65536 | 0.77 |

Shallow WS is **better** — flat-optimal from ~2–64, declining monotonically past
~256, ~27% slower by 65536. This refuted the handoff-amortization model *and* the
`WS = capacity` conclusion drawn from the burst bench.

**The real mechanism is cache, not wakeups.** In a saturated pipeline both threads
are always busy, so the "parking" cost is largely hidden — the controller wakes the
pump while the pump still has work to do. What dominates instead is **cache
locality**: a shallow WS hands the controller data the pump *just* wrote (still in
cache); a deep WS makes the controller read data the pump touched thousands of
entries ago, long evicted. So depth *hurts*. The handoff-amortization story had
predicted the §3 artifact for the wrong reason; only the artifact-free measurement
exposed both the model and the prior conclusion.

---

## 8. Two regimes, opposite answers

Both benchmarks are now legible, and they disagree because they measure different
workloads — which is itself the finding, not a contradiction to resolve:

| | shallow WS (16–64) | deep WS (= capacity) |
| --- | --- | --- |
| **Sustained continuous logging** (`queue_depth`) | **best** | ~12% slower (cache cold) |
| **Burst-then-flush** (`logger_benchmark` noop) | ~7–10% slower | **best** (no parking under the waiting flush) |
| **Worst-case in-flight memory** | capacity + WS | 2× capacity |

The burst-then-flush advantage for deep WS is real but narrow, and it leans on a
**flush** that, in the benchmark, is a *measurement device* (added to "measure the
full pipeline"), not real usage. Real continuous logging does not flush every N
entries, so the sustained regime is the more representative one — and it wants a
shallow WS. The original "shrink the second buffer" instinct from §1 was therefore
*right* (better sustained throughput *and* ~half the worst-case memory); it had been
rejected in §3 only because it was judged against the wrong regime.

---

## 9. Q — "How exactly was this conducted? Which variable was held fixed?"

A measurement is only as trustworthy as the reader's ability to see what was held
fixed. The two-buffer space admits three sweeps; attributing the effect correctly
requires all three:

| sweep | mailbox | WS | isolates | result |
| --- | --- | --- | --- | --- |
| 1. both together | = N | = N | nothing (conflated) | 1.0 M/s → 0.83 as N grows |
| 2. fixed mailbox, vary WS | 1024 | swept | **the WS** | shallow best; → 0.77 at 65536 |
| 3. fixed WS, vary mailbox | swept | 64 | **the mailbox** | flat ~1.0; 0.91 at 65536 |

Sweeps 1 and 2 had been run; sweep 3 — "the other way round" — had not, and the
conclusion needed it. Sweep 3 (WS pinned at 64, mailbox swept):

| mailbox (capacity) | throughput |
| --- | --- |
| 256 | 0.98 M/s |
| 1024 | 1.00 |
| 4096 | 1.00 |
| 16384 | 1.00 |
| 65536 | 0.91 |

The mailbox is **neutral** for throughput — flat from 256 to 16384, a small dip only
at 65536 (within or near noise). So the decline seen when *both* buffers grow
together is the WS, not the mailbox. This also confirms the §0 split is free: the
mailbox staying deep (`= capacity`) costs nothing for throughput and is required deep
anyway for burst absorption and policy. For completeness, the both-together sweep (no
flush): 256 → 1.02, 1024 → 1.04, 4096 → 1.01, 16384 → 0.91, 65536 → 0.83 M/s — the
same decline, now attributable to the WS half alone.

---

## 10. Q — "Is the knee absolute, or a fraction of capacity? Does the message count matter?"

This is the question that yields the *formula*. Sweep 2 used one mailbox base
(1024), so it cannot distinguish an **absolute** knee (a fixed entry count) from a
**fractional** one (a percentage of capacity). The M×W matrix can — sweep W at
several fixed mailbox bases (median throughput, M/s):

```text
                       mailbox (capacity) →
   WS HWM ↓     256      1024     4096     16384      ≈ f(W)
   16          1.09     1.00     1.13     1.10       ~1.08   ← flat-optimal
   64          0.98     1.02     1.00     1.02       ~1.00
   256         0.98     0.93     0.91     0.87       ~0.92   ← knee
   1024        0.89     0.91     0.92     0.92       ~0.91
   4096        0.87     0.87     0.83     0.91       ~0.87
```

Two readings:

- **Down each column** (fixed mailbox, vary W): throughput falls ~20% as W grows.
  W matters.
- **Across each row** (fixed W, vary mailbox): **flat** within noise. The mailbox
  does not matter.

So `throughput ≈ f(W)` **alone**. The decisive point: `W = 256` gives ~0.92 M/s
whether the mailbox is 256 (W = 100% of capacity) or 16384 (W = 1.5% of capacity).
If the knee were *fractional*, those would differ sharply; they're the same. **The
knee is absolute.** "x% of capacity" is the wrong frame — it would shrink W below the
optimum for small capacities and balloon it past the optimum for large ones, losing
throughput at both ends.

**Does the message count B matter?** For the sustained formula, no — it is a
**constant**, needing only `B ≫ W` to be in steady state, after which the per-entry
rate is B-independent. B re-enters *only* in the burst regime, as a threshold: if
`W ≥ B` the whole burst fits and the pump never parks; if `W < B`, a flush pays
≈ `(B/W) × wakeup` extra. Real continuous logging is `B → ∞` — always sustained — so
B is not a parameter of the default.

---

## 11. Pinning the knee — the fine grid

A fine W grid (mailbox fixed at 4096, 40 samples) locates the knee tightly:

| WS HWM | throughput | |
| --- | --- | --- |
| 32 | 0.99 M/s | (low edge — parking floor begins) |
| 48 | **1.06** | ┐ |
| 64 | **1.04** | │ flat-optimal |
| 96 | **1.04** | │ plateau |
| 128 | **1.03** | ┘ |
| 192 | 0.99 | ← decline begins |
| 256 | 0.99 | |
| 384 | 0.945 | |
| 512 | 0.86 | |
| 1024 | 0.93 | |

The plateau is **W ≈ 48–128**, the knee **~128–192**, with a slight softening
*below* 48 (the parking floor finally showing) — confirming a genuine plateau, not
"smaller is always better."

This **quantitatively confirms the cache mechanism.** `sizeof(FormattedEntry)` is
~250–270 B (two `String` headers, the inline `Meta` smallvec, an `Option<String>`),
so the WS VecDeque's contiguous footprint is `~256 B × W`:

```text
W = 128  →  ~32 KB  =  L1 data cache       ← exactly where the plateau ends
W_cache  =  L1_size / sizeof(FormattedEntry)  ≈  32 KB / 256 B  ≈  128
```

The knee *is* the L1 boundary. The constant that §1 guessed as 64 is, derived,
`L1 / sizeof(entry)` — and 64 sits one notch *below* it, which turns out to be the
right side to err on (a larger message lowers `W_cache`, so 64 stays inside the
plateau where 128 might creep over L1).

---

## 12. The derived model, and where it landed

```text
throughput_sustained(W)  ≈  T_max            for W ≲ W_cache
                            T_max − g(W)      for W ≳ W_cache   (g grows as W spills L1→L2→L3→RAM)

W_cache  ≈  L1_size / sizeof(FormattedEntry)         (this machine: ~128)
```

- **Independent of the mailbox / capacity** (matrix rows flat — §10).
- **Independent of B** once `B ≫ W` (§10); B is a threshold only in the burst regime.
- Both regime optima are **absolute counts**, not fractions:

```text
sustained (B → ∞):        W* ≈ W_cache (≈ 64 here)    → cache-warm handoff
burst + flush (you flush): W* ≈ B (the burst size)    → no parking under the flush
```

**Default: an absolute `~64`, expressed as `min(capacity, 64)`** — inside the
flat-optimal plateau, the shallowest such value (least memory), with a margin below
the L1 knee for larger entries. The `min(capacity, …)` exists only so a deliberately
tiny mailbox is never out-deepened by the WS. It is explicitly **not** a percentage
of capacity.

**Outcome.** Adopted as the default ([ADR 0007](adr/0007-shallow-writablestream-high-water-mark.md)):
`DEFAULT_WS_HWM = 64`, `resolve_ws_hwm(capacity) = capacity.min(64)`. The mailbox
stays `= capacity`, so `with_queue_capacity(N)` now means exactly N (worst-case
in-flight `N + 64`, not `2N`). The trade — ~12% sustained throughput and ~half the
worst-case memory, against ~7–10% burst-then-flush drain latency — is the one this
investigation set out to quantify. It lands separately from the arc-swap routing
change (ADR 0006), which deliberately left the WS HWM untouched pending this
measurement.

---

## Appendix — the artifact catalogue

A benchmark for an internal queue depth is unusually easy to fool. Each artifact
here *looked like a finding* at the time:

1. **The clamp.** `HWM = WS_PIPELINE_HWM.min(capacity)` made HWM > capacity
   unmeasurable, so the top of the swept range was mistaken for a plateau.
   *Lesson: verify the knob actually takes the values you believe you are sweeping.*
2. **The burst-size plateau.** A fixed N-entry burst makes throughput top out at
   `HWM ≥ N` for every workload, manufacturing a "plateau" at the burst size.
   *Lesson: to measure a steady-state property, the workload must dwarf the buffer,
   or the buffer absorbs the whole experiment.*
3. **The flush tail.** Flushing each sample adds a drain whose length scales with
   the buffer under test, confounding the very variable being swept.
   *Lesson: never put the independent variable into the teardown of a sample.*

And the meta-lesson that ties them together: the handoff-amortization model
**predicted the first, artifact-laden result**, which made a wrong model look
confirmed and nearly closed the investigation. A mechanism that matches a flawed
measurement is doubly dangerous — it removes the urge to look further. Only a
measurement designed against the artifacts (`Block`-throttled, no flush,
`BATCH ≫ HWM`, WS decoupled from capacity) revealed the true driver (cache locality)
and the true shape (an absolute knee at `L1 / sizeof(entry)`). The progression — guess
→ false confirmation → walk-back → artifact-free rebuild → reversal → isolation →
formula — is the template, not the constant `64`.
