# ADR 0004 — Two tasks per transport

**Status:** Accepted

## Context

Each transport in the pipeline runs behind two concurrent tasks:

1. **`slot_pump`** — drains the per-slot mailbox and calls
   `writer.enqueue_when_ready()` on the transport's `WritableStream` writer.
2. **`WritableStream` controller** — dequeues chunks from the stream's internal
   buffer and calls `sink.write()` (i.e. the transport's actual `Transport::log`
   I/O).

The question is whether these two tasks need to be separate, or whether one task
could own both the mailbox drain and the sink I/O.

## Decision

Keep them as two separate tasks, with the `WritableStream`'s internal queue as
the buffer between them.

## Rationale

`enqueue_when_ready` returns as soon as a chunk is accepted into the stream's
queue, not when the sink has processed it. This means the `slot_pump` can pull
the next entry off the mailbox and feed it into the queue while the
`WritableStream` controller is still blocked on `sink.write()` for the previous
entry — up to the queue's high-water mark. The two tasks pipeline through the
queue rather than serialising end-to-end.

If they were merged into one task, the combined task would have to `await
sink.write()` directly. It could not touch the mailbox again until the sink
finished. Every entry would be serialised through the sink's I/O latency with no
buffering. The `WritableStream` queue — and its high-water-mark backpressure
signal — would be bypassed entirely.

The split also preserves a clean layer boundary: the `slot_pump` understands the
logger's mailbox protocol (`Entry` / `Flush` / `Close` messages,
`OverflowPolicy`); the `WritableStream` controller understands the stream's
protocol (queuing strategy, sink lifecycle, abort). Neither needs to know about
the other's concerns.

## Consequences

- **Thread count with `default_spawner`**: 2 OS threads per transport (one per
  task). Each call to `SpawnFn` creates one `std::thread::spawn`. A logger with
  N transports holds 2N threads for its lifetime.
- **`single_threaded_spawner`** collapses all tasks onto one shared OS thread
  via a `FuturesUnordered` executor, at the cost of requiring non-blocking sinks.
- The `WritableStream` queue depth (bounded by HWM = `queue_capacity`) is the
  buffer that makes the pipelining meaningful. Shrinking `queue_capacity` to 1
  makes the two-task split pointless; enlarging it increases burst absorption.

## Alternatives considered

**Single task owning both mailbox drain and sink I/O directly.**  
The task would call `transport.log()` (or equivalent `sink.write()`) inline
after popping from the mailbox. Simpler — one task, one thread with
`default_spawner` — but every entry serialises through the sink's I/O latency.
Under a slow sink (network, disk), the mailbox fills at the rate of the sink,
not at the rate of the queue. Backpressure hits producers sooner and more
harshly than it needs to. The `WritableStream` abstraction would add no value
and could be removed entirely.

## References

- `winston/src/pipeline.rs` — `slot_pump`, `make_writer_builder`, `admit`.
- `winston/src/pipeline.rs` — `default_spawner`, `single_threaded_spawner`.
- ADR 0002 — Direct-dispatch backpressure (establishes the per-slot mailbox +
  pump model this ADR builds on).
