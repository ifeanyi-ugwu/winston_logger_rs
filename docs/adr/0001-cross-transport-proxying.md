# ADR 0001 — Cross-transport proxying

**Status:** Accepted

## Context

A logger writes entries to one or more *transports* (file, daily-rotating file,
HTTP endpoint, MongoDB, stdout/stderr). A recurring operational need is to
**move entries from one transport to another** — typically: keep logs locally
in a rolling file for speed and durability, then periodically ship them to a
remote/queryable store and reclaim local disk. The original codebase had a
`Proxy<T>` trait (`fn proxy(&self, target: &dyn Proxy<T>)`, `fn ingest(&self,
Vec<T>)`) plus a `ProxyTransport` that ran `source.proxy(target)` on a timer.

That trait was deleted during the move to the WHATWG-streams-based transport
contract (`Transport: WritableSink<LogInfo>`). This ADR records how proxying
was rebuilt on the new architecture, what was deliberately *not* built, and the
delivery guarantees that result.

The hard constraint is the standard distributed-systems one: moving data across
a process boundary that may crash, you can have **at-most-once** (delete source
before the target confirms — lossy on crash), **at-least-once** (target
confirms, then delete source — duplicates on crash-retry), or **exactly-once**
(= at-least-once **+ an idempotent receiver**, or a distributed transaction).
There is no fourth option, and the stream layer can't manufacture one.

## Decision

Proxying is decomposed into composable handles and a small glue layer, all in
`winston_transport::proxy` (helpers) and on the `Transport` trait (handles).

### Handles on the `Transport` trait

- `query_handle() -> Option<Box<dyn DynQueryHandle>>` — non-destructive read.
  `DynQueryHandle::query(&LogQuery) -> Option<Box<dyn DynReadableSource>>` opens
  a fresh streaming source per call. Implemented by `FileTransport`,
  `MongoDBTransport`. Survives the transport's move into a Logger (it holds
  cloned config — a path, a connection string — not the live writer).
- `ingest_handle() -> Option<Box<dyn DynIngestHandle>>` — the write side, as a
  handle. `DynIngestHandle::ingest(Vec<LogInfo>) -> BoxFuture<StreamResult<()>>`
  writes a batch to the underlying store, independent of the live `WritableSink`
  path. Implemented by `FileTransport`, `DailyRotateFile`, `HttpTransport`,
  `MongoDBTransport`. Skipped on stdout/stderr (`WriterTransport`) — not a
  sensible ingestion target.

### Transport-specific destructive reads

A destructive read is "give me the entries *and* remove them from the source"
— the rotation/archive pattern. The mechanics differ enough per store that
this is **not** a uniform trait method; each transport that supports it exposes
its own typed operation:

- `DailyRotateFile::rotation_handle().list_rotated_files()` — rotation is the
  transport's whole job; rotated files have predictable names, so the recovery
  state is just "the files on disk." Crash-recoverable by construction.
  `archive::ship_rotated_files` is the workflow helper (list → drain each,
  decompressing `.gz` → delete on success).
- `FileTransport::rotate_handle().rotate_and_drain(spawn)` → `FileDrain` —
  atomically renames the active file out of the way (the live writer's handle
  is swapped to a fresh file in the same critical section, so no entry straddles
  the boundary) and hands back a `ReadableStream` over the frozen renamed file.
  *Less* crash-recoverable than DailyRotate: a crash after the rename orphans
  the `*.drain-*` file — mitigated by `rotate_handle().list_pending_drains()`
  for startup recovery.
- `MongoDBQueryHandle::query_consuming(&LogQuery)` → `(source, MongoDBConsumeToken)`
  — the source records each document's `_id` as it emits; `token.delete_consumed(receipt)`
  then deletes exactly those documents. Precise: docs inserted after the cursor
  opened, or skipped by `start`/`limit`, are untouched.

### The glue (`winston_transport::proxy`)

- `pipe_to_ingest(source, target, batch_size, spawn) -> StreamResult<DrainReceipt>`
  — drain one source into one target, batched. The `DrainReceipt` is returned
  **only on full success**; on failure no receipt is produced and the entries
  already shipped stay shipped (at-least-once).
- `fan_out_to_ingests(source, &[targets], batch_size, spawn) -> StreamResult<FanOutStats>`
  — broadcast to N targets in parallel, bounded at one batch in flight.
  `FanOutStats::into_drain_receipt()` yields `Some` only if every target took
  the whole payload.
- `content_id(&LogInfo) -> String` — the dedup key: `sha256` of a *recursively
  key-sorted* JSON serialization of the entry. The recursive sort makes it
  independent of `HashMap` iteration order, `serde_json`'s `Map` backing, and
  the order a formatter wrote the meta fields — so re-reading a persisted entry
  and hashing it again yields the same key, which is exactly what a crash-retry
  re-ship needs.

### Source-deletion is gated on a receipt

`MongoDBConsumeToken::delete_consumed` *takes* a `DrainReceipt` (consuming it)
and rejects one whose `entries_shipped()` doesn't match the number of documents
the source emitted. This turns "delete the source before the ship completed" —
a silent data-loss footgun in the original `Proxy::proxy` — into a type error.
For the `ship_rotated_files` / `remove_file` path the same safety holds because
the delete is on the `Ok` branch of the same function.

### Idempotent targets

`MongoDBTransport::idempotent_ingest_handle()` keys each document on its
`content_id` as `_id` and uses `insert_many(ordered: false)`, treating an
all-duplicate-key error (code 11000) as success — so a re-ship after a crash
collides on `_id` and is a no-op. `HttpTransport` stamps `_id = content_id` on
every flat payload entry, so a dedup-aware endpoint can absorb retried POSTs;
an endpoint that ignores it is unaffected. A **flat file as a target cannot be
made idempotent** under append semantics (appending the same line twice gives
two lines, and a flat file has no key-based upsert) — this is a documented
limit, not a gap.

### The canonical recipe

`DailyRotateFile` (crash-recoverable source) → `MongoDBTransport::idempotent_ingest_handle`
(target), keyed on `content_id`, deleting the rotated file only after
`pipe_to_ingest` returns a `DrainReceipt`. A crash anywhere ⇒ the rotated file
survives ⇒ the next pass re-ships ⇒ upserts no-op what made it, insert the rest
⇒ converges. No loss, no net duplicates — given the operating rules below.

## Operating rules (the conditions exactly-once-ish relies on)

1. **Delete the source only after a `DrainReceipt`.** Enforced by the type
   system for `delete_consumed`; structurally for `ship_rotated_files`.
2. **One proxy per source.** Two processes draining the same collection/file
   ship overlapping sets — absorbed only by an idempotent target.
3. **No overlapping ship passes.** Pass N+1 must not start before pass N has
   deleted the file it's still draining. Serialize (process-local mutex / single
   scheduler).
4. **Flat-file targets are not idempotent.** Re-ship after a crash re-appends.
   For exactly-once the target must be a keyed store.
5. **Recover orphaned drains.** If you use `rotate_and_drain`, call
   `list_pending_drains()` at startup and re-ship anything left over.

## Alternatives considered and rejected

- **Keep the `Proxy<T>` trait.** It conflated two unrelated concerns —
  "atomic bulk transfer with source-side clear" and "batch ingestion endpoint."
  The second is just `WritableSink::write` (repeated, possibly buffered) in the
  new architecture; the first is transport-specific (file rename vs. cursor +
  delete vs. just-clear-a-buffer) and shouldn't be a lowest-common-denominator
  trait. Its only remaining real user (HTTP) implemented it solely to be an
  ingestion *target*, with `proxy()` returning an error.
- **A `DynDrainHandle` trait** unifying destructive reads. Same objection — the
  semantics differ too much; a uniform trait would force the wrong abstraction.
  Each transport with destructive support exposes its own typed method.
- **Stamp an `id` field onto `LogInfo` at creation.** Survives even
  non-canonical serializations, but it's a `logform`-level change with wide
  blast radius (every formatter, every consumer). A content-hash computed at
  proxy time is sufficient because once an entry is persisted as a JSON line its
  bytes are fixed, and `logform::json()` serializes canonically (sorted keys,
  serde_json's default `Map` backing) — re-reads hash the same.
- **Per-entry commit ("ship one, pop it from the source, ship the next").**
  Doesn't eliminate duplicates (a crash between "wrote to target" and "popped
  from source" still duplicates — an idempotent target is still required), and
  "pop the first entry from a file" is O(file size) per entry → O(n²) total.
  Batch-granularity commit with an idempotent target is correct and bounded.
- **Two-phase commit spanning the source-delete and the target-write.** The
  stream layer has no transaction coordinator, and most target stores don't
  participate in one. Out of scope.
- **A content-addressed-directory file target** (`<content_hash>.json` per
  entry, `create_new` ⇒ no-op on retry). This *is* idempotent — but it's a
  different artifact than a flat log file (millions of tiny files, slow bulk
  writes). Reasonable to offer later as an explicit `ContentAddressedDir`
  transport; not built on spec.
- **Push-based rotation hooks** (`on_rotated: Box<dyn Fn(PathBuf)>` on
  DailyRotate / a side-channel command to FileTransport's sink). Crash-unsafe
  (the callback's pending work is lost on crash, unlike a file on disk that the
  next pass re-discovers), couples shipping cadence to rotation cadence, and a
  slow callback running on the WritableStream's task thread blocks the live
  writer. Pull-based (`list_rotated_files` / `list_pending_drains` + your own
  timer) is crash-recoverable and cadence-independent.

## Consequences

- **At-least-once is the default** — data is never lost (given a crash-recoverable
  source and rule 1), but a crash-retry re-ships, so non-idempotent targets see
  duplicates.
- **Exactly-once is achievable** for keyed targets (MongoDB-upsert, dedup-aware
  HTTP server) via `content_id`, given the operating rules. A flat-file target
  is at-least-once, period.
- **`rotate_and_drain` is the weaker source** — it needs `list_pending_drains`
  startup recovery to match DailyRotate's crash-safety. Docs steer people to
  DailyRotate as the destructive source; `rotate_and_drain` is the escape hatch
  for "I really only have a plain file."
- **The destructive-proxy matrix** (source → target):

  |              | File | DailyRotate | Http | MongoDB | Writer |
  |--------------|------|-------------|------|---------|--------|
  | **File**         | ✓ | ✓ | ✓ | ✓ | ✗ |
  | **DailyRotate**  | ✓ | ✓ | ✓ | ✓ | ✗ |
  | **MongoDB**      | ✓ | ✓ | ✓ | ✓ | ✗ |
  | **Http**         | ✗ | ✗ | ✗ | ✗ | ✗ |
  | **Writer**       | ✗ | ✗ | ✗ | ✗ | ✗ |

  The `✗` rows/columns are fundamental: HTTP and stdout/stderr have no local
  store to read from (can't be sources); stdout/stderr isn't a sensible
  archive target.
- **No exhaustive A→B pairing tests.** Each role is tested in isolation
  (every destructive source, every ingest target) and the glue has
  at-least-once / idempotent **contract tests** plus one no-external-deps
  end-to-end test (`DailyRotate → real FileTransport ingest`). Mongo/HTTP
  pairings are env-gated examples, not CI tests. Re-verifying every cell would
  mostly re-test `pipe_to_ingest`, which is already covered.

## References

- `winston_transport::proxy` module docs — the operational guide.
- Contract tests: `winston_transport::proxy::tests::at_least_once_*`.
- Canonical-recipe example: `winston_daily_rotate_file/examples/ship_to_mongodb.rs`.
