# ADR 0008 — Two-pass format composition

**Status:** Accepted and implemented (decisions locked by the pressure-test below)

## Context

Formatting is currently **one-or-the-other**. `Routing::format_for` picks a
slot's `transport_format` *if it has one*, **else** the global format, else
passthrough — they are mutually exclusive:

```rust
fn format_for(&self, slot, entry) -> Option<FormattedEntry> {
    match (&slot.transport_format, &self.global_format) {
        (Some(tf), _)    => tf.apply(entry.clone()),   // transport REPLACES global
        (None, Some(gf)) => gf.apply(entry.clone()),
        (None, None)     => passthrough,
    }
}
```

The footgun: with

```text
global  = timestamp().finalize(json())
console = colorize().finalize(simple())
```

the console **loses the global `timestamp()` entirely** — to get it back, every
transport with its own format must re-add `timestamp()`. Global enrichment is not
actually global once a transport opts into formatting.

This is a **divergence from Winston**, not a faithful port of it. Verified against
`winston-transport`'s `_write`:

```js
TransportStream.prototype._write = function _write(info, enc, callback) {
  // ... silent / level filtering ...
  if (info && !this.format) {
    return this.log(info, callback);                              // (A) no transport format
  }
  const transformed = this.format.transform(Object.assign({}, info), this.format.options); // (B)
  return this.log(transformed, callback);
};
```

By the time `info` reaches a transport, the **logger format has already run
upstream** (the logger is a Transform that runs its own format, finalizer
included, so `info[Symbol.for('message')]` is already set) and pushed that single
enriched `info` to every transport. So:

- **(A) no transport format** → the logger-rendered `info` passes through verbatim
  (inherits the logger's `MESSAGE`).
- **(B) transport format** → it runs on a *shallow copy* of the already-enriched
  `info`. The transport's transforms see every property the logger added
  (`timestamp`, `label`, …, because `Object.assign` copied them); the transport's
  finalizer overwrites `MESSAGE`.

Winston **composes** the transport format on top of the global-enriched entry; it
does not replace it. The current port replaces. The footgun is the port's, not
Winston's.

## Decision

Adopt Winston's **compose** model, correcting the replace-semantics, with a
per-stage composition rule grounded in logform's `FormatPipeline { transforms,
finalizer }`:

- **Transforms compose, global first:**
  `effective.transforms = global.transforms ∘ transport.transforms`.
- **Finalizer is transport-authoritative *when the transport set any format at
  all*** (including a transforms-only format, i.e. finalizer `None`); a transport
  with **no** format inherits the global format entirely (transforms *and*
  finalizer).

| global | transport | effective transforms | effective finalizer |
|---|---|---|---|
| `timestamp() + json()` | *(none)* | timestamp | json — *inherit all* |
| `timestamp() + json()` | `colorize() + simple()` | timestamp ∘ colorize | simple |
| `timestamp() + json()` | `metadata().into_pipeline()` *(structured)* | timestamp ∘ metadata | **None** |
| `json()` *(bare finalizer)* | `colorize() + simple()` | colorize | simple |

Row 2 is the footgun fixed: the console keeps the global `timestamp` *and* adds
its `colorize`. Row 3 is the structured-sink case: the sink gets the global
`timestamp` in its `info` (so a structured store records it) but renders nothing.

### Why a single finalizer, not "run global then transport"

The intuitive "run both pipelines" cannot be literal: a `FormatPipeline` with a
finalizer outputs a `String`, and a transport's transforms operate on a
`LogInfo`, not a string. Transforming a finalized blob is the "finalizers write
into `message`" anti-pattern ADR 0005 rejected. So composition is per-stage:
**transforms concatenate; exactly one finalizer renders.**

### Why transport-authoritative (the structured-sink rule)

The naive alternative — "transport's finalizer, else fall back to the global's" —
**breaks the structured-sink invariant** (ADR 0005: a slot with no finalizer never
renders). A mongo/http sink that sets `metadata().into_pipeline()` to
enrich-but-not-render would inherit the global `json()` finalizer and render a
string it discards. So a transport that set *any* format owns its finalizer
decision, `None` included; only a no-format transport inherits the global one. The
small price — a transport that wants "add a transform but keep the global's json
render" must re-state the finalizer — is paid by a rare case; the structured-sink
invariant is not negotiable.

## Relationship to Winston: deliberate divergences

This is a Winston *port*, so divergences are documented, not silent. Three, each
enabled by the Rust type design, each removing a Winston wart, and two of the
three are **observably identical** to Winston:

1. **Single finalizer** vs Winston's "both finalizers, last-write-wins on
   `MESSAGE`." Winston renders `MESSAGE` at the logger stage, then the transport
   overwrites it — the logger's render is thrown away. This model renders once.
   **Observably identical**, because a logform `Finalizer` is
   `fn finalize(&LogInfo) -> Option<String>`: it takes `&LogInfo`, so it *cannot*
   mutate the entry — its only effect is the returned string, and only the last
   one is ever used. Running the global finalizer and discarding it is provably
   side-effect-free waste. Possible only because the finalizer type is pure where
   Winston mutates `info[MESSAGE]` in place.
2. **Structured-sink-safe finalizer rule.** Winston leaves the logger's rendered
   `MESSAGE` on a transforms-only transport format, and structured sinks ignore
   it. **Observably identical** (the sink stores the same `info`), but the explicit
   `FormattedEntry { info, rendered }` split lets this port *skip* a render Winston
   is forced to perform and discard.
3. **Per-transport deep clone.** `format_for` deep-clones `LogInfo` per slot (the
   `Meta` smallvec and every `Value`). Winston's `Object.assign({}, info)` is
   *shallow*, so `Symbol.for('splat')` and nested meta are shared across
   transports and a mutating transform in one can corrupt another. This avoids a
   real Winston bug.

### The auditable trace

Entry `info("hello", { user: "alice" })` → `{ level:"info", message:"hello",
meta:{ user:"alice" } }`. `timestamp()` adds `meta.timestamp="10:00"`; `json()`
renders the object; `colorize()` colors `level` (`⟨g⟩info⟨/⟩`); `simple()` renders
`⟨level⟩: ⟨message⟩ ⟨meta-as-json⟩`; `metadata()` nests non-core fields. The
asymmetry: Winston renders the global finalizer **first, upstream, always**; this
model renders **once, at the end**.

| # | global | transport | output | same? | renders (Winston / Rust) |
|---|---|---|---|---|---|
| 1 | `ts+json` | none | `{"level":"info","message":"hello","user":"alice","timestamp":"10:00"}` | **same** | 1 / 1 |
| 2 | `ts+json` | `color+simple` | `⟨g⟩info⟨/⟩: hello {"user":"alice","timestamp":"10:00"}` | **same** | 2 / 1 |
| 3 | `ts+json` | `metadata`-only *(structured)* | stored `{ level, message, metadata:{ user, timestamp } }` | **same** | 1 / 0 |
| 4 | `json` | `color+simple` | `⟨g⟩info⟨/⟩: hello {"user":"alice"}` | **same** | 2 / 1 |
| 5 | `json` | `color`-only *(string sink)* | W: un-colorized json · R: colorized `Display` | **different** | 2 / 1 |

All four design rows are observably Winston-faithful, and this model does strictly
less work on three of them (Winston's wasted global render). **Row 5 is the one
output divergence**, and it generalizes to two classes:

- **Transforms-only transport format on a *string* sink, when the global has a
  finalizer.** Winston renders the global finalizer *before* the transport
  transform runs, so the transform is wasted and the un-rendered-by-transport
  output is the global's. This model defers rendering, so the transport transform
  is honored and the global finalizer is dropped. (Structured sinks are immune —
  they read `info`, which is identical.)
- **A transport transform that reads the *rendered string*** (`uncolorize` strips
  ANSI from `info[MESSAGE]`; custom transforms that inspect it). Winston's
  transport transforms run *after* the global finalizer, so a rendered `MESSAGE`
  exists to read; here the global was never rendered, so there is nothing to read.

Both classes are the same root cause — single-render vs render-at-every-stage —
and the same trade that buys the skipped renders in rows 2–4.

## Validation — the finalizer-rule pressure-test

Before building, the rule was swept across *every* shape, not just the four design
rows. A `FormatPipeline` is one of four shapes — `None`, transforms-only
(`Gt`/`Tt`), finalizer-only (`Gf`/`Tf`), both (`Gtf`/`Ttf`). Effective
`(transforms, finalizer)` for all sixteen `global × transport` combinations:

| T ↓ \ G → | `None` | `Gt` (transforms) | `Gf` (finalizer) | `Gtf` (both) |
|---|---|---|---|---|
| **`None`** | passthrough | `(Gt, —)` | `(—, Gf)` | `(Gt, Gf)` *inherit* |
| **`Tt`** (transforms-only) | `(Tt, —)` | `(Gt∘Tt, —)` | `(Tt, —)` ⚠ | `(Gt∘Tt, —)` ⚠ |
| **`Tf`** (finalizer-only) | `(—, Tf)` | `(Gt, Tf)` ✦ | `(—, Tf)` | `(Gt, Tf)` |
| **`Ttf`** (both) | `(Tt, Tf)` | `(Gt∘Tt, Tf)` | `(Tt, Tf)` | `(Gt∘Tt, Tf)` |

- **`Tf` / `Ttf` rows** (transport has its own finalizer): global transforms ride
  along, transport finalizer wins — all clean, all the footgun-fix. The `✦` cell
  (`Gt` global + `Tf` transport) is the pattern Winston users reach for: a global
  `timestamp().into_pipeline()` of shared enrichment, each transport picking its
  own finalizer (`console=simple`, `file=json`).
- **`None` row**: pure inheritance.
- **The two `⚠` cells** — `Tt` (transforms-only transport) over a global that *has*
  a finalizer (`Gf`/`Gtf`): the global finalizer is **dropped**. The only cells
  that need thought.

Drilling `Tt × Gtf` — global `timestamp()+json()`, transport
`colorize().into_pipeline()`:

- **Structured sink:** `(timestamp∘colorize, None)` → `rendered: None`, reads
  `info` — gets the timestamp, no wasted render. Correct, and the point of the
  rule.
- **String sink:** same effective → `Display` fallback (colorized, not json). This
  is Row 5.

The settling point: **this is not a regression.** Today's replace-semantics
already drops the global finalizer in this cell — `(colorize, None)`; the rule's
only *change* is that it now **adds** the global transforms (`Gt∘Tt`). So the cell
goes from "drops everything global" → "keeps global transforms, drops global
finalizer" — strictly better than current, still divergent from Winston.
**Accepted, documented.** No sink-type knob: the rule cannot tell a string sink
from a structured one (the sink chooses what to read), and trying to would
re-introduce the structured-sink render waste.

**Verdict:** sixteen shapes, exactly two cells need thought, both
defensible-and-documented, no case produces wrong output. The rule holds.

Three things the sweep surfaced to document (not decide):

- A structured sink with **no** format inherits the global finalizer → renders a
  string it ignores. The structured-sinks-never-render guarantee is *opt-in*: it
  requires the sink to carry a transforms-only effective format. Same as today.
- **Reconfigure freshness** (implementation): the effective pipeline depends on
  `global_format`, which `configure` can swap without rebuilding slots — so compute
  it in `format_for` (always fresh; composition is just assembling `Arc` handles)
  or invalidate a per-slot cache on global change.
- **Level gating** uses the pre-format level; a transform that rewrites `level`
  does not affect the gate. Pre-existing, unchanged by two-pass.

## Invariants

1. **Structured sinks never render** (ADR 0005) — preserved by the
   transport-authoritative finalizer rule; a transforms-only transport format
   yields `rendered: None`.
2. **Per-transport isolation** — each slot formats its own deep clone; no transform
   in one transport can affect another.
3. **Output matches Winston** except where a transport's *transform* would have
   observed, or been clobbered by, the global's intermediate render — a corner that
   exists only because of Winston's redundant double-render, which this model
   removes by design.

## Consequences

- **Breaking change, narrow.** Behavior changes only when the global format has
  **transforms** *and* a transport has its own format. A bare-finalizer global
  (`format: json()`, the common setup) → **no change** (nothing to compose). Global
  with transforms + a transport format → that transport now also gets the global
  transforms — almost always the author's intent. Ship with a changelog note.
- **Renders stay per-slot** until Layer 2 of the dedup (below): Row 1 with three
  inherit-transports renders 3× here vs Winston's 1×. The *transform* passes,
  however, drop to one with Layer 1, which lands with this change.
- **logform** grows a small `compose`/`then` that builds the effective pipeline per
  the rule — exposing the `transforms` / `finalizer` seam it already has.
- **`format_for`** builds the effective pipeline from `global_format` +
  `slot.transport_format`. The composition can be **precomputed per slot** and
  recomputed only when `global_format` changes (`configure`, rare), so there is no
  per-`log()` cost.

### The dedup: two layers, one bundled and one deferred

`format_for` runs per slot, so the **global transforms run once per transport**.
For *deterministic* transforms (`timestamp`, `label`) that is redundant work plus
microsecond-divergent timestamps; for a *non-deterministic* global transform — a
sampling / rate-limit filter that returns `None` to drop — it is an
**inconsistency**: the entry can be dropped on one transport and kept on another.
So deduping the global stage is not purely a perf optimization — its absence is a
**correctness wart**, which is why it splits into two layers with different
schedules:

- **Layer 1 — the two-stage split (bundled with this change).** Run the global
  transforms **once**, before the fan-out, producing one enriched `LogInfo`; then
  per slot run only the transport transforms + finalize. Buys **one timestamp**
  per event across all transports, **one** decision from a non-deterministic
  global transform (consistency from day one), and **N−1 fewer** global-transform
  passes. A global-transform drop up front cleanly skips every slot; a transport
  transform drop skips only its slot. Cheap, and it removes the wart — so it lands
  *with* the composition semantics, not after.
- **Layer 2 — sharing without re-cloning or re-rendering (deferred, not a
  blocker).** With the enriched info — and, for a pure inherit-slot, the whole
  rendered `FormattedEntry` — behind an `Arc`, those slots *borrow* instead of
  cloning and re-rendering (Winston's "render upstream once, fan out the same
  info"). This needs `FormattedEntry.info` (or `FormattedEntry` itself) to become
  `Arc`-shared, which ripples to every sink, so it is its own change — taken when
  convenient, with no correctness consequence to deferring it.

  Measured before building (`logger_benchmark`'s `fanout_render` vs
  `fanout_passthrough`): the per-slot render is ~30–58% of per-log cost, and only
  at 4–8 inherit-transports — rare. The dominant multi-transport cost is the
  fan-out coordination, which Layer 2 does not address; and the no-sink-ripple
  form of Layer 2 only trades renders for `String` clones (marginal, and a slight
  regression at one transport). So Layer 2 stays deferred until a many-inherit-
  transport workload makes the render-dedup pay.

Neither layer is the rejected finalize-at-pump (ADR 0006): rendering stays on the
caller thread; only the *timing* of the shared transform stage moves (once, up
front, vs N times in the loop). No pump-funnel.

## Alternatives considered and rejected

- **Replace semantics (current).** The footgun above; diverges from Winston.
- **Finalizer fallback-to-global.** Simpler, but renders for structured sinks that
  explicitly opted out — breaks ADR 0005's invariant.
- **Literal two-pass (Winston's double-render).** Renders the global finalizer then
  the transport's; the first is discarded. logform's pure `&LogInfo → String`
  finalizer makes the single-render output-equivalent, so the double render is pure
  waste.
- **"Replace, don't compose" opt-out.** Winston has none, and the case is already
  served by setting a full transport pipeline. Deferred unless a real need appears.
- **Fully deferring the dedup.** Rejected for Layer 1: shipping per-slot global
  transforms leaves the consistency wart for non-deterministic global transforms
  (see the pressure-test). Layer 1 is bundled; only Layer 2 (the `Arc` clone /
  re-render elimination) is deferred, since deferring it has no correctness cost.

## Decisions (locked by the pressure-test)

- **Transform order** — global-then-transport (shared context like `timestamp` /
  `label` established before transport-specific `colorize`). **Accepted.**
- **Finalizer rule** — transport-authoritative-when-present, else inherit.
  **Accepted** — confirmed sound by the sixteen-shape sweep; the two `⚠` cells are
  better-than-current and documented divergences from Winston, not regressions.
- **No opt-out** — a transport that wants replace-semantics sets a full pipeline.
  **Accepted.**
- **Dedup** — Layer 1 (two-stage split) **bundled** with this change, for
  consistency from day one; Layer 2 (`Arc`-in-`FormattedEntry`) **deferred** as a
  non-blocking follow-up.

**Implemented.** `Routing::dispatch_entry` runs the global transforms once
(stage 1) and `Routing::format_for` composes the transport stage on the enriched
entry per the rule (stage 2); logform exposes `FormatPipeline::transform` /
`::finalize` to split a pipeline across the two stages. Layer 2
(`Arc`-in-`FormattedEntry`) remains the deferred follow-up. The three divergences
are breaking and recorded in the landing commit.

## References

- `winston-transport` `lib/winston-transport/index.js` — `_write` (the composition
  mechanism verified above).
- `logform/src/finalizer.rs` — `FormatPipeline { transforms, finalizer }` and the
  `transform` / `finalize` stage methods; `Finalizer::finalize(&LogInfo) -> Option<String>`.
- `winston/src/pipeline.rs` — `Routing::dispatch_entry` (stage 1) and
  `Routing::format_for` (stage 2).
- ADR 0005 — LogInfo data model and the FormattedEntry boundary (the
  transform/finalizer split and the structured-sink invariant this builds on).
- ADR 0006 — Lock-free routing (why finalize-at-pump was rejected; the dedup here
  is render-on-caller and distinct from it).
