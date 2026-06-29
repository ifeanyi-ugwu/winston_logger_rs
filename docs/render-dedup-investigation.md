# Investigation — is the render-dedup (ADR 0008 Layer 2) worth building?

**Status:** Investigation log / measured-and-deferred (supports
[ADR 0008](adr/0008-two-pass-format-composition.md), the Layer 2 follow-up)

## How to read this

ADR 0008 split the global-transform dedup into two layers: **Layer 1** (run the
global transforms once — bundled, for correctness) and **Layer 2** (also share the
*rendered* result across transports that inherit the global format, so they neither
re-clone nor re-render). Layer 2 was deferred. This records the implementation
analysis and the benchmark that confirm deferring is the right call — and, just as
importantly, the re-run that would overturn it if the workload changes.

The lesson is the same one this codebase keeps relearning: a one-line "optimization"
in a design doc is a hypothesis, not a result. Measure what it buys before paying for
it.

All figures are from one machine; absolute times are machine-specific. The *shape*
(how cost scales with transport count) and the *render share* are the durable parts.

## 1. The question

Layer 2's pitch is "render upstream once, fan out the same info" — Winston's
efficiency. Is it worth building?

## 2. What Layer 2 actually helps — narrower than the one-liner

Thinking the implementation through first, before measuring:

- It only helps when **several transports all inherit the global format** and each
  re-renders it — e.g. console + file + http all on the default `json()`. A
  transport with its **own** format can't share (its output differs); a **single**
  inherit-transport has nothing to dedup.
- The **no-sink-ripple** form — render the inherit entry once, then hand it to each
  inherit-slot — only *trades* renders for `String` clones of the rendered output
  (each slot's mailbox owns its message, so it clones the shared entry). That is
  marginal at low transport counts, and it even *adds* a clone at **one** transport,
  so it needs an inherit-slot-count branch to avoid regressing the common case.
  Real complexity for a win that is zero at N=1.
- The **full** form — no clone, no re-render — needs the enriched `info`, or the
  whole `FormattedEntry`, behind an `Arc`, which **ripples into every sink's `write`
  signature** (`WritableSink<FormattedEntry>` → `WritableSink<Arc<FormattedEntry>>`,
  or `FormattedEntry.info: Arc<LogInfo>`).

So the real question reduces to: **does caller throughput actually degrade as you
add inherit-transports under a rendering format?** If yes, the render-dedup pays
off; if it's flat, it does not.

## 3. The measurement

`winston/benches/logger_benchmark.rs`, two fan-out groups that build a logger with
K transports (all `NoOp` sinks) and time `1000 × log() + flush`:

- **`fanout_render`** — the default `json()` global format, so every inherit-slot
  renders the entry per slot.
- **`fanout_passthrough`** — a `passthrough().into_pipeline()` global (no render),
  identical otherwise.

The second isolates the per-slot **render** (the cost Layer 2 removes) from the
K-way **fan-out coordination** (per-transport threads, the flush barrier) that both
share. If the two scale the same, render is not the cost.

## 4. The data

| K (transports) | `fanout_render` (json) | `fanout_passthrough` | render's share |
| --- | --- | --- | --- |
| 1 | 1.08 ms | 1.04 ms | ~0% |
| 2 | 1.97 ms | 1.40 ms | ~29% |
| 4 | 5.30 ms | 2.18 ms | ~58% |
| 8 | 24.5 ms | 12.7 ms | ~48% |

(`fanout_render` K=8 is **22.7×** K=1; `fanout_passthrough` K=8 is **12.2×** K=1.)

## 5. Findings

1. **Render is a real component** — ~30–58% of per-log cost at K≥2 inherit-
   transports. Layer 2's render-dedup would remove something real.
2. **But it is not the headline cost.** Passthrough — with the render entirely
   removed — *still* scales super-linearly (K=8 = 12× K=1). So the dominant cost is
   the **fan-out coordination**, not the render, and Layer 2 does not touch it.
   (That super-linearity is also partly the per-iteration `flush` — the same
   measurement artifact caught in the queue-depth investigation, where a benchmark's
   flush is a measurement device, not representative steady-state usage.)
3. **The win is rare.** It is meaningful only at K=4–8 inherit-transports; most
   loggers run 1–3, where the render share is ~0–29%.

## 6. Verdict — defer

Layer 2 is not worth building now:

- The clean (no-ripple) form is marginal — it trades renders for string clones and
  regresses the single-transport case.
- The full form ripples `Arc<FormattedEntry>` into every sink for a win that only
  materializes on a rare many-inherit-transport workload.
- It does not address the actual dominant multi-transport cost (fan-out
  coordination), which is itself partly a flush artifact and may be a non-issue in
  steady state.

So Layer 2 stays deferred, as ADR 0008 already had it — now evidence-backed rather
than assumed. Layer 1 (the consistency-critical half) shipped with the two-pass
change; only this render-sharing half is parked.

## 7. What would overturn this

Re-run `cargo bench --bench logger_benchmark -- "fanout_render|fanout_passthrough"`
and rebuild Layer 2 if **both** hold:

- The deployment runs **many inherit-transports** (≥4 transports all using the
  global format, no per-transport format) under **hot** logging, and
- the **render share** (the gap between `fanout_render` and `fanout_passthrough`) is
  a large fraction of per-log cost at that transport count.

Build order if it pays off: the **no-sink-ripple** contained form first (render the
inherit entry once, clone into each inherit-slot's mailbox, with the
inherit-slot-count branch to spare N=1); escalate to the **`Arc<FormattedEntry>`**
form only if the per-slot string clones it leaves behind also show up in the
numbers. And first confirm the fan-out super-linearity is real steady-state cost,
not the flush artifact (a no-flush, sustained fan-out bench), since that — not the
render — is the larger lever.

## References

- `winston/src/pipeline.rs` — `Routing::dispatch_entry` / `format_for` (where Layer 2
  would hoist the inherit render out of the slot loop).
- `winston/benches/logger_benchmark.rs` — `fanout_render`, `fanout_passthrough`.
- ADR 0008 — Two-pass format composition (Layer 1 bundled; Layer 2 deferred).
- `docs/queue-depth-investigation.md` — the sibling investigation and the
  flush-as-measurement-artifact lesson reused here.
