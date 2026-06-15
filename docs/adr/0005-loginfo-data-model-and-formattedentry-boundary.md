# ADR 0005 — LogInfo data model and the FormattedEntry boundary

**Status:** Proposed

## Context

`LogInfo` is one struct used across three phases of an entry's life, and the
shape that suits one phase taxes the others.

```rust
pub struct LogInfo {
    pub level: String,
    pub message: String,
    pub meta: HashMap<String, Value>,
    pub formatted: Option<String>,   // set only by finalizer formats
}
```

Three problems follow from this single shape.

**1. `formatted` is dead weight for most of an entry's life.** Formatting is
per-slot: `StateSnapshot::format_for` picks `slot.transport_format`, else
`global_format`, else passthrough, and produces a clone with `formatted` set
right before the mailbox push. So `formatted` is `None` from construction
through dispatch and only becomes `Some` in the per-slot clone a transport
actually receives. The field exists on every entry at every phase to carry a
value meaningful in exactly one of them. `Display` branches on it on every
read, and nothing at the type level stops a transport from being handed an
entry whose finalizer never ran.

**2. The two transport kinds want different things, and one nullable field
serves neither cleanly.** Reading the sinks:

- **String sinks** — console, `winston_file`, `winston_daily_rotate_file` —
  consume the entry through `Display`, i.e. they read `formatted`.
- **Structured sinks** — `winston_mongodb`, `winston_http` — consume through
  `to_flat_value()`, read `level` / `message` / `meta`, and never touch
  `formatted`.

A structured sink that receives a pre-rendered string has paid to render
something it discards. The current shape hides this because finalizers are
opt-in per slot, but it also means the type carries a render slot that half the
ecosystem has no use for.

**3. `HashMap<String, Value>` allocates on every call, and throws away the fact
that keys are static.** Every `LogInfo` heap-allocates a hash table plus a
`String` per key. Yet every metadata key in this codebase originates as a
`&'static str` and is then force-allocated:

- `log!` / `meta!` macros: `stringify!($key)` → `&'static str` → `with_meta`
- log-backend adapter: `"timestamp".to_string()`, `"target"`, `"file"`, `"line"`
- `winston_tracing`: `field.name()` returns `&'static str`, then `.to_string()`

The median log call carries a handful of short, static keys, and pays a table
allocation plus per-key allocations for all of them.

## Decision

Split `LogInfo`'s three jobs across the type system:

- `LogInfo` becomes the **pure data model** — construction, query, and
  serialization. It loses `formatted`. Its `meta` becomes a `Meta` newtype that
  keeps small, static-keyed metadata allocation-free.
- A new `FormattedEntry` is the **transport write boundary**. It carries the
  finalized `LogInfo` plus an optional rendered string.
- The `Format` trait keeps the **transform** role; a new `Finalizer` trait owns
  the **render** role. A pipeline is a transform chain plus an optional
  finalizer.

The read/query path stays pure `LogInfo`: `Logger::query`, `query_sync`,
`DynReadableSource`, and the query handles are untouched. Only the write
boundary moves to `FormattedEntry`.

### `LogInfo` and `Meta`

```rust
pub struct LogInfo {
    pub level: String,
    pub message: String,
    pub meta: Meta,
}

pub struct Meta(SmallVec<[(Cow<'static, str>, Value); 4]>);
```

- **`Cow<'static, str>` keys.** `&'static str` keys — every macro-generated key,
  every log-backend literal, every tracing field name — become `Cow::Borrowed`
  and cost zero allocation. Dynamically built keys (`format!("key_{i}")`) become
  `Cow::Owned` and allocate, as they must. `with_meta<K: Into<Cow<'static,
  str>>>` accepts both. This is compile-time key interning for the case that
  dominates this codebase, with no runtime intern table.
- **`SmallVec<[_; 4]>` inline storage.** The common 0–4 field entry carries its
  metadata inline; no table allocation. Entries with more than four fields spill
  to the heap, as a `HashMap` always would.
- **`Meta` exposes a map-like surface** (`get`, `insert`, `remove`, `iter`,
  `is_empty`, `contains_key`). `insert` overwrites on duplicate key — same
  semantics as the `HashMap` it replaces — by scanning the inline slice, which
  is cheap at this size. Iteration is insertion-ordered (a `HashMap` was
  unordered), which makes formatter output deterministic.
- **Custom `Serialize` / `Deserialize`** keep the exact JSON-object wire shape.
  Deserialized keys are `Cow::Owned` (they aren't static).

### `FormattedEntry`

```rust
pub struct FormattedEntry {
    pub info: LogInfo,
    pub rendered: Option<String>,   // Some only when a finalizer ran
}
```

`Display` moves here from `LogInfo`: emit `rendered` if present, else fall back
to `level` + `message` + `meta`. String sinks read it through `Display`;
structured sinks read `entry.info`. `SlotMessage::Entry` carries a
`FormattedEntry` (owned, one per slot — each slot's finalizer renders its own
string). `format_for` returns `FormattedEntry`.

A structured sink has no finalizer on its slot, so `rendered` is `None` and no
string is ever produced for it. **Structured sinks pay nothing for rendering, by
construction** — this is the property the whole split exists to guarantee.

### `Format` / `Finalizer` split

`formatted` had nowhere to live on `LogInfo`, so a finalizer can no longer be a
`Format<Input = LogInfo>` that sets a field. The render role separates into its
own terminal trait:

```rust
// Transform: unchanged role, still chainable.
pub trait Format { type Input; fn transform(&self, input: Self::Input) -> Option<Self::Input>; /* chain */ }

// Finalizer: terminal, produces the rendered string.
pub trait Finalizer: Send + Sync {
    fn finalize(&self, info: &LogInfo) -> Option<String>;
}
```

- **Transforms** keep `Format<Input = LogInfo>`: `timestamp`, `colorize`,
  `label`, `metadata`, `ms`, `pad_levels`, `align`, `uncolorize`.
- **Finalizers** move to `Finalizer`: `json`, `simple`, `printf`, `cli`,
  `logstash`, `pretty_print`.
- A **`FinalizeExt`** blanket trait terminates any transform chain with any
  finalizer:

  ```rust
  pub trait FinalizeExt: Format<Input = LogInfo> + Sized {
      fn finalize<F: Finalizer>(self, f: F) -> FormatPipeline;
  }
  impl<T: Format<Input = LogInfo>> FinalizeExt for T {}
  ```

  `finalize` is generic over `F: Finalizer`, so a user's own finalizer is a
  first-class chain terminator — `timestamp().finalize(my_finalizer())` — with no
  privilege for the built-ins. There is deliberately **no per-finalizer sugar**
  (`.json()`, `.simple()`): a method named after each built-in could not be
  generated for an arbitrary user finalizer, so it would make the built-ins
  first-class and every user finalizer second-class. One uniform `.finalize(f)`.

- **A bare finalizer is a complete format, through the same uniform path.** A
  blanket lifts *any* finalizer — built-in or user — into a pipeline with
  identity transforms, so `format: json()` and `format: my_finalizer()` are
  symmetric:

  ```rust
  pub trait IntoFormatPipeline { fn into_format_pipeline(self) -> FormatPipeline; }
  impl<F: Finalizer + Send + Sync + 'static> IntoFormatPipeline for F { /* identity transforms */ }
  ```

  This blanket cannot ride `std::convert::From` — `impl<F: Finalizer> From<F> for
  FormatPipeline` collides with core's reflexive `From<T> for T` — hence the
  dedicated `IntoFormatPipeline` trait.

The stored per-logger and per-transport format is a concrete pipeline holding an
optional transform chain and an optional finalizer, mapping `LogInfo` to
`FormattedEntry`:

```rust
pub struct FormatPipeline {
    transforms: Option<Arc<dyn Format<Input = LogInfo> + Send + Sync>>,
    finalizer:  Option<Arc<dyn Finalizer>>,
}
// apply: run transforms (if any) → run finalizer (if any) → FormattedEntry
```

- transform-only → `FormattedEntry { info: transformed, rendered: None }`
- chain + finalizer → `FormattedEntry { info, rendered: Some(..) }`
- bare finalizer → identity transforms + finalizer

A transform-only pipeline still applies its transforms to a structured sink's
`info` (e.g. `timestamp()` enriching `meta`) without rendering a string. Because
the bare-conversion blanket can cover only one of `Format` / `Finalizer` (Rust
coherence forbids both), the finalizer side takes the implicit path — it is the
common and user-extensible case — and a transform-only format terminates
explicitly: `timestamp().into_format_pipeline()`.

## Invariants

1. **Structured sinks never render.** No finalizer on a slot ⇒ `rendered: None`
   ⇒ zero string allocation for that transport. The split is pointless if a
   future change forces a default finalizer onto every slot; it must not.
2. **The write boundary is `FormattedEntry`; the read boundary is `LogInfo`.**
   Transports write `FormattedEntry`; query handles and readable sources stay
   `LogInfo`. A pre-format `LogInfo` cannot reach a sink's `write`.
3. **A bare `Finalizer` is a usable `format`, with no built-in privilege.**
   `format: json()` and `format: my_finalizer()` for a user-defined finalizer
   must both work without hand-constructing a pipeline, through the same
   `IntoFormatPipeline` blanket. No per-finalizer sugar method that a user
   finalizer could not also receive.
4. **`Cow::Borrowed` keys stay borrowed.** The hot path must not `.to_string()`
   a static key back into an owned allocation anywhere between `with_meta` and
   serialization.

## Alternatives considered and rejected

- **Typestate marker `LogInfo<Phase>`.** A `PhantomData<Phase>` distinguishes
  `Raw` from `Finalized`, sinks take `LogInfo<Finalized>`. Gives a compile-time
  "transport sees a finalized entry" guarantee, but the parameter propagates
  through every bound, every transport `impl`, and every test, and it keeps the
  `Option<String>` field — the guarantee is weak (a finalizer can still leave it
  `None`) for a large, pervasive cost. The honest distinction is structured vs
  string, not raw vs finalized; the marker encodes the wrong axis.

- **`FormattedEntry { info, formatted: String }` (always rendered).** Makes the
  string non-optional and forces a default finalizer to run for every slot. A
  structured sink then pays to render a string it discards — re-introducing the
  exact waste this ADR removes, for the half of the ecosystem that is
  structured. Rejected for violating invariant 1.

- **Model two sink kinds as distinct traits** (`StringSink` consuming `String`,
  `StructuredSink` consuming `LogInfo`), with typed per-slot mailboxes and the
  pipeline rendering only for string slots. The most honest model and marginally
  the most efficient — string slots never carry structure, structured slots
  never carry a string. Rejected for now on blast radius: it requires the format
  chain's output type to become a per-slot associated type, splits the
  `Transport` trait, and reworks the uniform `SlotMessage` dispatch loop. The
  `FormattedEntry` boundary captures the decisive property (structured sinks
  don't render) at a fraction of the cost. Revisit if a sink ever needs to *not*
  carry `info` at all.

- **Runtime string interning for keys** (`lasso` / `ustr` — a global
  `Map<str, id>`). A real additional win for keys that are long *and* highly
  repeated, but needs a concurrency story and an eviction policy: unbounded
  distinct keys (`format!("key_{i}")`) leak the table. `Cow<'static, str>`
  already makes the static keys that dominate here zero-allocation without any
  table. Deferred; only worth revisiting if profiling shows owned-key allocation
  dominating, which requires keys both `> 24` bytes and dynamically generated and
  repeated — a corner absent from this codebase.

- **Closure-deferred lazy metadata** (tracing's model: store field exprs, format
  only if a subscriber asks). winston's `with_meta(k, v: Into<Value>)` evaluates
  values eagerly at the call site, so capturing them lazily means redesigning the
  macro surface to hold closures. Out of scope: the achievable win here is
  eliminating the *container* and *key* allocations, which `Meta` does; value
  evaluation stays eager to keep the ergonomic API.

- **Keep one `Format` trait, have finalizers write into `message`.** With
  `formatted` gone, a finalizer would have to overwrite `message` with its
  rendered blob. Structured sinks downstream then see a JSON-string `message`
  instead of structured fields. Conflates the two channels; rejected.

## Consequences

- **Breaking change across the workspace.** `LogInfo` loses `formatted` and
  changes `meta`'s type; `Transport` becomes `WritableSink<FormattedEntry>`; the
  finalizer constructors change trait. Affected: `logform`, `winston_transport`,
  `winston` (pipeline + console), `winston_file`, `winston_daily_rotate_file`,
  `winston_mongodb`, `winston_http`, `winston_tracing`. The crates are pre-1.0
  (`-dev`); migration is mechanical per site.

- **Format-construction sites migrate** `…chain(json())` → `…finalize(json())`.
  Bare `format: json()` is unchanged.

- **Metadata access migrates** from `HashMap` methods to the `Meta` surface.
  Most call sites (`get`, `insert`, `remove`, `iter`, `is_empty`) are
  source-compatible; direct `HashMap`-typed bindings change to `Meta`.

- **Median allocation drops.** A typical entry (few static keys, ≤ 4 fields)
  goes from "hash table + N key strings" to zero metadata-container/key
  allocation. Value contents (`Value::String`) still allocate — inherent without
  interning values, which is not worthwhile.

- **`Display` cost drops for un-rendered reads** — no `Option` branch on
  `LogInfo`; the branch lives on `FormattedEntry` where rendering is the point.

- **Deterministic formatter output** as a side effect of insertion-ordered
  `Meta`.

### Migration sequence

Each commit compiles and is independently reviewable:

1. `feat(logform)!: Meta newtype` — swap `HashMap` → `Meta` behind a
   map-compatible surface; `formatted` stays. Isolates the metadata change.
2. `feat(logform)!: split Finalizer from Format` — add `Finalizer`,
   `FinalizeExt`, `FormatPipeline`; move the six renderers; `formatted` set via
   a shim.
3. `feat!: FormattedEntry boundary` — drop `formatted` from `LogInfo`, introduce
   `FormattedEntry`, flip `Transport` / `SlotMessage` / `format_for`, migrate all
   sinks. The breaking flip, once.
4. `docs: mark ADR 0005 Accepted`.

## References

- `logform/src/log_info.rs` — `LogInfo`, `Display`, serialization.
- `logform/src/formats/format.rs` — `Format` trait and `chain`.
- `logform/src/formats/{json,simple,printf,cli,logstash,pretty_print}.rs` — the
  finalizers that move to `Finalizer`.
- `winston_transport/src/transport.rs` — `Transport: WritableSink<LogInfo>`, to
  become `WritableSink<FormattedEntry>`.
- `winston/src/pipeline.rs` — `StateSnapshot::format_for`, `SlotMessage`.
- ADR 0002 — Direct-dispatch backpressure (the per-slot mailbox + format-per-slot
  model this builds on).
