# Investigation — is the render-dedup (ADR 0008 Layer 2) worth building?

**Status:** Investigation log / measured-and-deferred (supports
[ADR 0008](adr/0008-two-pass-format-composition.md), the Layer 2 follow-up)

## How to read this

ADR 0008 split the global-format dedup into two layers: **Layer 1** (run the global
*transforms* once — bundled, for correctness) and **Layer 2** (also render the
*output* once and share it across transports that inherit the global format).
Layer 2 was deferred. This page is both the plain-language explanation of what
Layer 2 is and the benchmark evidence for deferring it — so the deferral is a
decision that can be re-run, not a guess.

The recurring lesson: a one-line "optimization" in a design doc is a hypothesis.
Measure what it buys before paying for it.

Figures are from one machine; absolute times are machine-specific. The *shape* (how
cost scales with transport count) and the *render share* are the durable parts.

## What Layer 2 was — the repeated work it targets

It is about **wasted repeated work when several transports all use the same (global)
format.**

Log to three sinks — console, file, http — where none sets its own format, so all
three **inherit** the global `timestamp().json()`. They produce *identical* output.
But dispatch today (after Layer 1, which already runs the global transforms once)
does this:

```text
log("hello")    global = timestamp().json();  console + file + http all inherit
  │
  ├─ STAGE 1 (Layer 1, already built):  timestamp(entry) → enriched   ← transforms run ONCE ✓
  │
  └─ STAGE 2, once PER slot:
        console:  clone enriched → json-render → FormattedEntry → mailbox
        file:     clone enriched → json-render → FormattedEntry → mailbox
        http:     clone enriched → json-render → FormattedEntry → mailbox
                  └──────────────────┬──────────────────┘
                     3 identical clones + 3 identical json renders
```

All three render the *same* json string independently. That repeated render is the
waste Layer 2 targets — render once, then fan out:

```text
log("hello")
  ├─ STAGE 1:    timestamp(entry) → enriched
  ├─ STAGE 1.5:  json-render(enriched) → ONE FormattedEntry       ← render ONCE (Layer 2)
  └─ fan it out:  console · file · http  all get that one entry
```

So in one sentence: **Layer 1 runs the global transforms once; Layer 2 would render
the output once.** Same idea, one stage later.

## Why "share it" isn't free

Each transport has its **own mailbox** that *owns* its message — the same object
cannot have three owners. Two ways out, each with a cost:

**(a) Clone the rendered entry into each mailbox** — no sink changes:

```text
   render once → [FormattedEntry] ──clone──▶ console mailbox
                                  ──clone──▶ file mailbox
                                  ──clone──▶ http mailbox
```

This swaps "3 renders" for "1 render + 3 **clones**." A clone (copy the finished
string) is cheaper than a render (build the string), so it is a win — but a small
one. And at **one** transport it is "1 render + 1 clone" vs today's "1 render" →
*slightly worse*, so it needs a special case for N=1.

**(b) Share by `Arc` — no clone, no re-render** (the real win):

```text
   render once → Arc<FormattedEntry> ──cheap──▶ console · file · http
```

But sinks consume `FormattedEntry` *by value*; passing an `Arc` changes **every
sink's `write` signature** (`WritableSink<FormattedEntry>` →
`WritableSink<Arc<FormattedEntry>>`) — a ripple through every transport crate.

## Was the render even the bottleneck? The measurement

`winston/benches/logger_benchmark.rs` builds a logger with K `NoOp`-sink transports
and times `1000 × log() + flush`, two ways:

- **`fanout_render`** — the default `json()` global, so each inherit-slot renders.
- **`fanout_passthrough`** — `passthrough().into_pipeline()` (no render), otherwise
  identical.

The passthrough run **removes the render entirely**, so the gap between the two is
exactly the render — the only thing Layer 2 could remove. Everything else (the K-way
fan-out: per-transport threads, the flush barrier) is shared by both.

| K (transports) | `fanout_render` (json) | `fanout_passthrough` | render's share |
| --- | --- | --- | --- |
| 1 | 1.08 ms | 1.04 ms | ~0% |
| 2 | 1.97 ms | 1.40 ms | ~29% |
| 4 | 5.30 ms | 2.18 ms | ~58% |
| 8 | 24.5 ms | 12.7 ms | ~48% |

The tell is the **passthrough** column: with render gone, cost *still* explodes with
K (12× at K=8). So the dominant cost is the **fan-out itself**, not the render.
Layer 2 only removes the gap:

```text
  per-log cost at K=8 inherit-transports:

  json         ████████████████████████  24.5 ms
                           └ render ~11.8 ms ┘   ← all Layer 2 can remove
  passthrough  ████████████              12.7 ms  ← the floor; fan-out, untouched by Layer 2
```

Even at its best (K=8) Layer 2 roughly *halves* the cost — but the 12.7 ms fan-out
floor remains, and that floor is itself partly a **flush artifact** of the benchmark
(the per-iteration flush is a measurement device, not steady-state usage — the same
trap caught in the queue-depth investigation), not a real steady-state cost.

## Findings — why it was deferred

- **Rare win.** It only helps with **4–8 transports all inheriting the global
  format**. Most loggers run 1–3, where the render share is ~0–29%.
- **The clean version is marginal** — it trades renders for string clones, and
  regresses the single-transport case.
- **The full version ripples** `Arc<FormattedEntry>` into every sink for that
  rare-workload win.
- **It does not fix the actual bottleneck** — the fan-out coordination, which
  dominates and is itself partly a measurement artifact.

So Layer 2 stays deferred, as ADR 0008 already had it — now evidence-backed rather
than assumed. Layer 1 (the consistency-critical half) shipped with the two-pass
change; only this render-sharing half is parked.

## When to revisit

Re-run one command —
`cargo bench --bench logger_benchmark -- "fanout_render|fanout_passthrough"` — and
rebuild Layer 2 if **both** hold:

- the deployment runs **many inherit-transports** (≥4, all on the global format,
  none with a per-transport format) under **hot** logging, and
- the **render gap** (the distance between the two columns) is a large fraction of
  per-log cost at that transport count.

Build order: the **no-sink-ripple** form (a) first — render once, clone into each
inherit-slot's mailbox, with the N=1 branch — and escalate to the
**`Arc<FormattedEntry>`** form (b) only if the per-slot string clones it leaves
behind also show up in the numbers. The fan-out super-linearity — the larger lever
this investigation surfaced — was chased down separately and fixed:
`docs/spawner-oversubscription-investigation.md` (it is thread oversubscription
under `default_spawner`, addressed by `pooled_spawner`).

## References

- `winston/src/pipeline.rs` — `Routing::dispatch_entry` / `format_for` (where Layer 2
  would hoist the inherit render out of the slot loop).
- `winston/benches/logger_benchmark.rs` — `fanout_render`, `fanout_passthrough`.
- ADR 0008 — Two-pass format composition (Layer 1 bundled; Layer 2 deferred).
- `docs/queue-depth-investigation.md` — the sibling investigation and the
  flush-as-measurement-artifact lesson reused here.
