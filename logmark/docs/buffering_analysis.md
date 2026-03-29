# Rust Logging Libraries and File I/O: A Source-Level Analysis of Buffering Strategies

**Type:** Technical Analysis
**Codebase:** [logmark](../) — a benchmark harness comparing Rust logging libraries
**Logmark commit:** `38f03c0c9d4d341e5ddca3c85b07d978a6bec543`
**Libraries covered:** env_logger 0.11.5, fern 0.6.2, tracing 0.1.40 + tracing-subscriber 0.3.18,
tracing-appender 0.2.3, slog 2.7.0 + slog-json 2.6.1 + slog-async 2.8.0, winston 0.8.0
**Winston repo:** [ifeanyi-ugwu/winston_logger_rs](https://github.com/ifeanyi-ugwu/winston_logger_rs) (commit `50cdceb8c389fdbdef0aaf0c1acde1ec2a20a1d4`)
**Test environment:** macOS 12.x, Apple M-series, release build (`cargo build --release`)
**Author:** ifeanyi ugwu

---

## Abstract

A file-target logging benchmark does not measure what most people think it measures. What it
measures is the combination of the logger's core serialization cost and its I/O strategy. If two
loggers produce the same output but one issues one `write()` syscall per record and the other
issues twenty-nine, you are not comparing loggers — you are comparing buffering philosophies. The
difference in observed throughput between those two strategies on macOS APFS is approximately 25×.

This analysis opens every library's source, locates the exact function that issues file writes,
counts the syscalls, and explains the mechanism. It then uses those findings to reason about what
a fair benchmark setup requires, why the `PrebufDrain` fix applied to slog is not an artificial
advantage, what winston's current I/O path costs and what a one-line change would do to it, and
what remains unresolved in slog-async.

---

## 1. Background and Motivation

### 1.1 What a file-target benchmark actually measures

A logging benchmark typically runs a fixed number of iterations, measures elapsed wall time, and
reports operations per second. For the sink and stdout targets, the number is dominated by the
logger's internal serialization work — formatting, struct traversal, locking — because the actual
write either goes nowhere (sink) or to a buffered terminal (stdout with OS line buffering).

The file target is different. Writing to a regular file on a local filesystem invokes the kernel
on every `write()` call. The kernel copies the data into the page cache, marks the page dirty, and
returns. That round-trip costs roughly **2–5 µs on macOS APFS** and **1–3 µs on Linux ext4**,
regardless of how many bytes you write. A single `write("{\n")` costs as much as
`write("{\"level\":\"INFO\",\"msg\":\"A logging message...\"}\n")` because the cost is the
syscall overhead, not the data transfer (the page cache absorbs the data instantly).

When a logger issues fifteen separate `write()` calls to compose a single JSON record — one for
`{`, one for each field key, one per separator, one per value, one for `}`, one for `\n` — it pays
that 2–5 µs fifteen times. A logger that assembles the entire line into a buffer first and calls
`write()` once pays it once. For 100,000 iterations, that gap compounds to seconds.

### 1.2 The `write()` syscall and user-space buffering

The standard mitigation is **user-space buffering**: accumulate bytes in a `Vec<u8>`, `String`, or
`BufWriter<File>` until there is enough data to make a single kernel call worthwhile, then flush.
There are two natural granularities for logging:

**Per-record buffering.** Format the entire log line into an owned buffer before touching the file.
Issue one `write_all(&buf)` at the end. This is what env_logger, tracing, and the post-fix slog
(`PrebufDrain`) all do.

**Cross-record buffering.** Wrap the file in a `BufWriter<File>` and let writes accumulate across
multiple records until the buffer fills (default 8 KB) or the writer is dropped/flushed. This
amortizes the syscall cost across ~150–200 records at typical log line lengths. Fern does this.

The worst case is **no buffering**: a streaming serializer (like serde_json's `Serializer`) is
pointed directly at a raw `File`. Every intermediate `write_all` call in the serializer — one per
JSON field, per delimiter, per quote character — hits the kernel immediately. This is what
slog-json + raw `File` does, and it is the root cause of the 25× gap observed in this benchmark.

### 1.3 Three actors, different responsibilities

For async loggers an additional dimension matters: **who pays the syscall cost and when**.

- **Synchronous logger:** the calling thread pays every syscall inline, blocking the caller until
  all writes are done.
- **Async logger, unbuffered worker:** the calling thread pays a cheap channel send. The worker
  thread pays the syscalls. But if you measure total throughput by waiting for the worker to drain
  (e.g. via `drop(logger)`), the total number of syscalls is unchanged — you have only moved them
  to a different thread.
- **Async logger, buffered worker:** the calling thread pays a cheap channel send. The worker
  issues one syscall per record. Total throughput is now determined by the worker's I/O rate, but
  since that rate is far higher (fewer syscalls), the wall time is much lower.

This distinction matters when comparing slog-async (async + unbuffered worker) against
tracing-appender (async + effectively buffered worker). Their async architectures look similar on
the surface. Their file performance is not.

---

## 2. The Libraries, Examined from Source

### 2.1 env_logger

**Source:** `env_logger-0.11.5/src/fmt/mod.rs`

```rust
// line 131
pub struct Formatter {
    buf: Rc<RefCell<Buffer>>,
    // ...
}

// line 182
fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    self.buf.borrow_mut().write(buf)       // writes into the inner Buffer (Vec<u8>)
}

// line 149
writer.print(&self.buf.borrow())           // single write_all to the actual target
```

The `Formatter` passed to your custom format closure is not the file — it is a `Buffer`, which is
a `Vec<u8>` borrowed from an internal writer pool. Every `write!` or `writeln!` call in your
closure appends bytes to this Vec. When the closure returns, `writer.print(&self.buf.borrow())`
issues **one** `write_all()` against the actual output target (file, stdout, pipe), then clears
the buffer.

`Buffer` is not publicly documented as a `Vec<u8>`, but the `Formatter::write` implementation
shows it clearly: all writes go into `self.buf.borrow_mut()`, which is the in-memory buffer.

**Syscalls per record (file target): 1.**
The format closure can call `write!` as many times as it likes; none of those calls touch the
file. Only `writer.print()` does.

**Additional overhead:** none beyond the format closure itself. No timestamp unless explicitly
added by the user's format closure.

---

### 2.2 fern

**Source:** `fern-0.6.2/src/log_impl.rs`

```rust
// line 99
pub struct File {
    pub stream: Mutex<BufWriter<fs::File>>,
}
```

```rust
// line 574 (inside impl Log for File)
// Formatting first prevents deadlocks on file-logging,
// ...
let mut writer = self.stream.lock().unwrap_or_else(|e| e.into_inner());
```

Fern's file output type wraps the `fs::File` in a `BufWriter` before wrapping it in a `Mutex`.
The `BufWriter` has a default capacity of 8 KB. The log implementation formats the message
_before_ acquiring the Mutex lock — the comment on line 574 says explicitly: "Formatting first
prevents deadlocks on file-logging." Once the formatted string is ready, the Mutex is locked,
and the formatted bytes are written to the `BufWriter`.

The `BufWriter` accumulates these bytes in its internal buffer. It issues a `write_all` to the
underlying `File` only when its 8 KB buffer fills, or when it is flushed/dropped. At a typical
log line length of ~60 bytes, this means one syscall per approximately **130 records**.

**Syscalls per record (file target): ~1 per 130 records** (amortized; ~0.008 syscalls/record).
**Lock hold time:** only the buffered write — the expensive formatting work happens before the
lock is acquired.
**Additional overhead:** none. Fern's format closure provides a `format_args!` result; no
additional timestamp or allocation is added by the library itself.

---

### 2.3 tracing (synchronous)

**Source:** `tracing-subscriber-0.3.18/src/fmt/fmt_layer.rs`

```rust
// line 944
fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
    thread_local! {
        static BUF: RefCell<String> = RefCell::new(String::new());   // line 946
    }

    BUF.with(|buf| {
        // ... format entire event into buf via format_event() ...

        // line 975
        let res = io::Write::write_all(&mut writer, buf.as_bytes());

        buf.clear();   // line 991
    });
}
```

The tracing subscriber uses a **thread-local `String`** named `BUF` as a per-call scratch buffer.
The entire event — including the serde_json serialization of all fields, the level, the target,
and any configured timestamp — is written into `BUF` first. The `serde_json::Serializer` used
inside `format_event` streams into this `String`, not into the file. Only after formatting is
complete does `write_all(&mut writer, buf.as_bytes())` issue the single file write.

The thread-local avoids re-allocating the `String` on every call: `buf.clear()` resets the length
to zero while retaining the allocated capacity, so subsequent events reuse the same heap memory
until a particularly long message causes a reallocation.

This design means `serde_json`'s many intermediate `write_str` calls are all in-memory string
appends (O(1) amortized), and the file writer receives **one contiguous byte slice** per event.

**Syscalls per record (file target): 1.**
**Thread-local reuse:** yes — the `String` buffer capacity grows to the high-water mark and stays
there, eliminating most per-event allocations.
**Additional overhead:** the serde_json JSON format used by `.json()` includes a timestamp via
`time::OffsetDateTime::now_utc()` plus RFC3339 formatting. This is a clock syscall + string
allocation per event.

---

### 2.4 tracing-appender (NonBlocking / async)

**Sources:**
`tracing-appender-0.2.3/src/non_blocking.rs`
`tracing-appender-0.2.3/src/worker.rs`

The tracing-appender `NonBlocking` writer implements `std::io::Write` in a specific way:

```rust
// non_blocking.rs, line 244
impl std::io::Write for NonBlocking {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // ...
        return match self.channel.send(Msg::Line(buf.to_vec())) {   // line 252
            Ok(_) => Ok(buf_size),
            Err(_) => Err(io::Error::from(io::ErrorKind::Other)),
        };
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {        // line 265
        self.write(buf).map(|_| ())
    }
}
```

Every `write_all` call on the `NonBlocking` writer allocates a `Vec<u8>` clone of the buffer and
sends it as a `Msg::Line` through a bounded crossbeam channel. The key question is therefore:
**how many times does the tracing subscriber call `write_all` on this writer per event?**

The answer comes from `fmt_layer.rs:975` (covered in §2.3): the subscriber formats the entire
event into a thread-local `String` and then calls `write_all` exactly **once** with the complete
JSON line. The `NonBlocking` writer therefore receives **one channel send per event**, carrying
one `Vec<u8>` containing the complete line.

The worker thread is implemented in `worker.rs`:

```rust
// worker.rs, line 56
pub(crate) fn work(&mut self) -> io::Result<WorkerState> {
    // Block until at least one message arrives.
    let mut worker_state = self.handle_recv(&self.receiver.recv())?;

    // Drain all currently available messages without blocking.
    while worker_state == WorkerState::Continue {
        let try_recv_result = self.receiver.try_recv();
        worker_state = self.handle_try_recv(&try_recv_result)?;
    }
    self.writer.flush()?;   // flush once per batch
    Ok(worker_state)
}
```

```rust
// worker.rs, line 32
fn handle_recv(&mut self, result: &Result<Msg, RecvError>) -> io::Result<WorkerState> {
    match result {
        Ok(Msg::Line(msg)) => {
            self.writer.write_all(msg)?;   // line 33 — one file write per message
            Ok(WorkerState::Continue)
        }
        // ...
    }
}
```

The worker blocks on the first available message, then drains all remaining messages with
`try_recv()` before calling `flush()`. During a high-throughput benchmark where the channel fills
faster than the worker drains it, this batch drains many records back-to-back before a single
`flush()`. Each message still gets its own `write_all()` to the file, but since each message is
already the complete JSON line, **one `write_all()` = one record = one syscall**.

**Caller-side cost per record:** 1 `Vec<u8>` allocation + 1 channel send.
**Worker-side syscalls per record: 1.**
**Total syscalls for 100K records: 100K** (same as sync tracing).
**Latency benefit:** the caller does not block on the file write. Enqueue latency (P50/P99) is
dominated by the channel send, not the syscall. Total throughput is unchanged vs the sync version;
the benefit is purely in call-site latency distribution.

---

### 2.5 slog + slog-json (synchronous, before fix)

**Sources:**
`slog-json-2.6.1/src/lib.rs`
`slog-async-2.8.0/lib.rs` (for context)

```rust
// slog-json-2.6.1/src/lib.rs, line 178
pub struct Json<W: io::Write> {
    newlines: bool,
    flush: bool,
    values: Vec<OwnedKVList>,
    io: RefCell<W>,          // <— the writer is behind a RefCell, not Mutex
    pretty: bool,
}
```

```rust
// line 234
impl<W> slog::Drain for Json<W> where W: io::Write {
    fn log(&self, rinfo: &Record, logger_values: &OwnedKVList) -> io::Result<()> {
        let mut io = self.io.borrow_mut();                           // line 239
        let mut serializer = serde_json::Serializer::new(&mut *io); // line 245
        self.log_impl(&mut serializer, rinfo, logger_values)?;
        // ...
        if self.newlines {
            io.write_all("\n".as_bytes())?;   // line 250
        }
    }
}
```

`serde_json::Serializer::new(&mut *io)` creates a streaming serializer whose backing writer is
the `RefCell`-borrowed inner writer — directly. In the file target, that inner writer is
`fs::File`. Every intermediate write that serde_json issues during serialization goes **directly
to the file**, with no buffering layer in between.

The `RefCell<W>` design (versus `Arc<Mutex<W>>`) is not a mistake — `RefCell` is correct for a
drain that is not shared across threads. The outer `std::sync::Mutex` wrapping (as used in the
benchmark setup and in slog-json's own documentation example) provides thread safety, but adds no
buffering.

#### Counting serde_json's write calls per record

`serde_json::CompactFormatter` maps each structural element of the JSON output to a distinct
`write_all` call on the underlying writer:

| Action                           | Call                          |
| -------------------------------- | ----------------------------- |
| `begin_object`                   | `write_all(b"{")`             |
| `begin_object_key` (after first) | `write_all(b",")`             |
| `begin_string` (for key)         | `write_all(b"\"")`            |
| key bytes                        | `write_all(key.as_bytes())`   |
| `end_string` (for key)           | `write_all(b"\"")`            |
| `begin_object_value`             | `write_all(b":")`             |
| `begin_string` (for value)       | `write_all(b"\"")`            |
| value bytes                      | `write_all(value.as_bytes())` |
| `end_string` (for value)         | `write_all(b"\"")`            |
| `end_object`                     | `write_all(b"}")`             |
| newline                          | `write_all(b"\n")`            |

With `add_default_keys()` adding three fields (`ts`, `level`, `msg`), and accounting for the
opening/closing braces and comma separators, the total lands at approximately **27–30 `write_all`
calls per record**.

`add_default_keys()` also adds:

```rust
// slog-json-2.6.1/src/lib.rs, line 334
"ts" => FnValue(move |_ : &Record| {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}),
```

This is a **clock syscall** (`clock_gettime` under the hood) plus an RFC3339 string allocation on
every record. At 100K iterations this adds ~50–100 ms beyond the file write cost, and it is an
overhead that env_logger, fern, and our benchmark's custom format closures do not pay.

**Syscalls per record (file target): ~27–30.**
**Measured elapsed time for 100K records: ~8.1 s** (median across 3 runs).
**Root cause:** serde_json's streaming serializer writes to the file directly; no buffering layer
exists between the serializer and `fs::File`.

---

### 2.6 slog + PrebufDrain (synchronous, after fix)

**Source:** `logmark/src/main.rs` (this repository)

The fix separates serialization from I/O by introducing a custom `slog::Drain` implementation:

```rust
struct PrebufDrain<W: Write + Send> {
    writer: Mutex<W>,
}

impl<W: Write + Send + 'static> slog::Drain for PrebufDrain<W> {
    type Ok = ();
    type Err = slog::Never;

    fn log(&self, record: &slog::Record, values: &slog::OwnedKVList) -> Result<(), slog::Never> {
        // --- Phase 1: serialize entirely into a Vec<u8> ---
        // No lock held. No file touched. All writes are in-memory Vec appends.
        struct KvSer<'a>(&'a mut Vec<u8>);
        impl slog::Serializer for KvSer<'_> {
            fn emit_arguments(&mut self, key: slog::Key, val: &std::fmt::Arguments) -> slog::Result {
                write!(self.0, ",\"{}\":\"{}\"", key, val).ok();
                Ok(())
            }
        }

        let mut buf = Vec::with_capacity(256);
        write!(buf, "{{\"level\":\"{}\",\"msg\":\"{}\"",
               record.level().as_short_str(), record.msg()).ok();
        let mut ser = KvSer(&mut buf);
        record.kv().serialize(record, &mut ser).ok();
        values.serialize(record, &mut ser).ok();
        buf.extend_from_slice(b"}\n");

        // --- Phase 2: one locked write ---
        self.writer.lock().unwrap().write_all(&buf).ok();
        Ok(())
    }
}
```

Phase 1 uses slog's `Serializer` trait to walk the record's key-value pairs, writing each field
as a JSON key-value pair into a `Vec<u8>`. The `write!` macro into a `Vec<u8>` issues no syscalls
— it is a pure memory operation. Phase 2 acquires the `Mutex<W>` and calls `write_all` once with
the complete buffer.

This is structurally identical to what env_logger does with its `Buffer` and what tracing does
with its thread-local `String` — the mechanics differ, but the invariant is the same: exactly one
`write_all` call reaches the underlying writer per record.

The output format is also now consistent across all three targets: `{"level":"...","msg":"..."}`
with no timestamp, matching the format closures used by env_logger and fern in this benchmark.
The old `slog_json::Json<File>` + `add_default_keys()` setup was measuring a different workload
(timestamp computation + RFC3339 formatting + JSON streaming) in addition to different I/O
behaviour. Both differences are corrected by `PrebufDrain`.

**Syscalls per record (file target): 1.**
**Measured elapsed time for 100K records: ~0.35 s** (median across 3 runs).
**Improvement over the pre-fix setup: ~23×.**

#### Why this is not a cheat

The question a rigorous reader should ask: does `PrebufDrain` give slog an artificial advantage
that the other loggers do not have?

The answer is no — it gives slog the same behaviour that the other loggers already had by default.
env_logger buffers into a `Vec<u8>` before writing. Tracing-subscriber buffers into a thread-local
`String` before writing. Fern buffers across records with a `BufWriter`. None of these libraries
stream their serializer output directly to a raw `File`. The original slog-json configuration was
the outlier — it was the only one doing that. `PrebufDrain` brings slog into parity with the
established pattern, not above it.

---

### 2.7 slog-async

**Source:** `slog-async-2.8.0/lib.rs`

slog-async's calling-thread path converts the borrowed `slog::Record` (which has lifetime-bound
references to the caller's stack frame) into an owned `AsyncRecord` before sending it through the
channel:

```rust
// lib.rs, line 480
pub struct AsyncRecord {
    msg: String,                          // allocated from record.msg()
    level: Level,
    location: Box<slog::RecordLocation>, // heap-allocated
    tag: String,                          // allocated from record.tag()
    logger_values: OwnedKVList,           // Arc clone
    kv: Box<dyn KV + Send>,              // boxed KV chain
}

// line 491
pub fn from(record: &Record, logger_values: &OwnedKVList) -> Self {
    let mut ser = ToSendSerializer::new();
    record.kv().serialize(record, &mut ser)
          .expect("`ToSendSerializer` can't fail");

    AsyncRecord {
        msg: fmt::format(*record.msg()),   // line 499 — String allocation
        level: record.level(),
        location: Box::new(*record.location()),
        tag: String::from(record.tag()),
        logger_values: logger_values.clone(),
        kv: ser.finish(),
    }
}
```

The calling thread pays: one `String` allocation (the formatted message), one `Box` allocation
(the location), one `String` allocation (the tag), one `Arc` clone (the logger values), one
boxed KV chain, and one channel send. This is approximately **4–5 heap allocations + 1 channel
send** per log call.

The worker thread receives the `AsyncRecord` and calls `async_record.log_to(drain)`, which
reconstructs a `slog::Record` from the owned data and passes it to the underlying drain:

```rust
// line 516
drain.log(&Record::new(&rs, &format_args!("{}", self.msg), BorrowedKV(&self.kv)),
          &self.logger_values)
```

When the underlying drain is `slog_json::Json::default(log_file)` — as in the current benchmark
configuration — the worker thread runs serde_json's streaming serializer against a raw `File`,
issuing the same ~27–30 `write_all` calls per record as the synchronous slog case. The async
channel moved the serialization cost from the calling thread to the worker thread; it did not
reduce it.

**Caller-side cost per record:** ~4–5 allocations + 1 channel send.
**Worker-side syscalls per record: ~27–30** (identical to slog sync before fix).
**Measured elapsed time for 100K records: ~9.4 s** (median across 3 runs).

This is slightly _worse_ than the synchronous slog case (8.1 s) despite the async indirection,
for two reasons: the `AsyncRecord` allocations add caller-side overhead, and the channel
back-pressure (`Block` strategy, 50K capacity) causes the caller to stall whenever the worker
falls behind — which it does, because the worker is bottlenecked on file I/O.

#### The fix (pending)

The fix is the same as for slog sync: replace `slog_json::Json::default(log_file)` with
`PrebufDrain::new(log_file)` as the drain passed to `slog_async::Async::new()`. The worker would
then serialize into a `Vec<u8>` and issue one `write_all` per record. Projected improvement:
~27× reduction in worker syscalls, bringing slog-async file performance in line with
tracing-appender.

```rust
// current (unfixed)
let drain = slog_json::Json::default(log_file).fuse();
slog_async::Async::new(drain).build().fuse()

// fix
let drain = PrebufDrain::new(log_file).fuse();
slog_async::Async::new(drain).build().fuse()
```

---

### 2.8 winston

**Sources:**
`logform/src/log_info.rs`
`winston_transport/src/transport_adapters.rs`

winston's `WriterTransport<W, L>` wraps any `W: Write` in a `Mutex` and issues writes via
`writeln!`:

```rust
// transport_adapters.rs, line 198
pub fn new(writer: W) -> Self {
    Self { writer: Mutex::new(writer), _phantom: PhantomData }
}

// line 211
fn log(&self, info: L) {
    if let Ok(mut writer) = self.writer.lock() {
        let _ = writeln!(writer, "{}", info);
    }
}
```

`writeln!(writer, "{}", info)` expands to a `write_fmt` call, which invokes `io::Write::write_all`
once for the `Display` output of `info` and once for the newline `\n`. The `Display`
implementation of `LogInfo` is:

```rust
// logform/src/log_info.rs, line 131
impl fmt::Display for LogInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)   // line 136
    }
}
```

`self.message` is a `String` that was formatted upstream (by the caller thread, before the log
entry was sent through the async channel). It is already a complete, ready-to-write string. The
`Display` implementation writes it as a single `write_str` call, which maps to one `write_all`.
The newline adds a second `write_all`.

**Syscalls per record (file target): 2.**
**Note:** there is no `BufWriter` wrapping the `File`. Each of the two calls reaches the OS.

This is better than old slog (27–30 syscalls) but worse than the per-record-buffered loggers (1
syscall). More precisely, at 100K records: 200K file-write syscalls at ~3 µs each = ~600 ms of
pure I/O cost, plus channel overhead and any format transform work in the worker.

#### What adding BufWriter would do

`WriterTransport::new(writer: W)` is fully generic over `W: Write`. The `BufWriter<File>`
implements `Write`. Therefore the fix requires no changes to winston's library code:

```rust
// current
builder.transport(winston::transports::WriterTransport::new(log_file))

// with buffering
builder.transport(winston::transports::WriterTransport::new(BufWriter::new(log_file)))
```

With a `BufWriter<File>` (default 8 KB capacity), the two `write_all` calls per record land in
the buffer. At ~60 bytes per log line, the buffer holds approximately 130 records before it needs
to flush. Over 100K records, **200K syscalls become ~770 syscalls** — a ~260× reduction in I/O
pressure. The total elapsed time for the file target would be expected to drop from ~925 ms to
within the range of fern (~335 ms), since both would be BufWriter-backed with similar line sizes.

The latency picture is different from throughput: because winston is async, the worker's writes
are off the critical path for the calling thread. Call-site P50/P99 latency is dominated by the
channel send, not the file write — and adding `BufWriter` does not change that. What changes is
total throughput (measured by the time from the first `log!` call to `drop(logger)` completing).

---

## 3. Comparative Summary

The following table summarises the file-target I/O strategy for each library. "Syscalls/record"
refers to the number of `write()` system calls issued to the file per log record.

| Library                    | User-space buffer               | Syscalls/record             | Notes                                                               |
| -------------------------- | ------------------------------- | --------------------------- | ------------------------------------------------------------------- |
| fern                       | `Mutex<BufWriter<File>>`        | ~0.008 (1 per ~130 records) | Format-before-lock; BufWriter amortises across records              |
| env_logger                 | `Vec<u8>` per record            | 1                           | Format into Vec, single `write_all`                                 |
| tracing (sync)             | thread-local `String` (reused)  | 1                           | serde_json streams into String; one `write_all` to file             |
| tracing-appender           | thread-local `String` → channel | 1 (in worker)               | Caller pays 1 channel send; worker pays 1 `write_all`               |
| slog + PrebufDrain         | `Vec<u8>` per record            | 1                           | Same pattern as env_logger; this repo's fix                         |
| winston (current)          | none                            | 2                           | message bytes + `\n`; no BufWriter                                  |
| winston (with BufWriter)   | `Mutex<BufWriter<File>>`        | ~0.008                      | Drop-in fix: `WriterTransport::new(BufWriter::new(file))`           |
| slog + slog-json (unfixed) | none                            | ~27–30                      | serde_json streams to raw File; timestamp syscall per record        |
| slog-async (unfixed)       | none (in worker)                | ~27–30 (in worker)          | AsyncRecord serialization in worker; fix: swap drain to PrebufDrain |

**Benchmark results (macOS, Apple M-series, 100K iterations, release build):**

| Library                    | File target — elapsed (median) | Ops/s  |
| -------------------------- | ------------------------------ | ------ |
| fern                       | 0.337 s                        | ~297K  |
| slog + PrebufDrain         | 0.350 s                        | ~286K  |
| env_logger                 | 0.377 s                        | ~265K  |
| tracing-appender           | 0.328 s                        | ~305K  |
| tracing (sync)             | 0.649 s                        | ~154K  |
| winston                    | 0.925 s                        | ~108K  |
| slog-async (unfixed)       | 9.380 s                        | ~10.7K |
| slog + slog-json (unfixed) | 8.105 s                        | ~12.3K |

The variance between tracing sync (0.649 s) and tracing-appender (0.328 s) is purely latency
hiding: the total syscall count is identical (100K), but tracing-appender overlaps them with the
caller loop via the async channel. The difference between tracing sync and env_logger (0.649 s vs
0.377 s) likely reflects the additional fields and timestamp that tracing's JSON formatter
includes by default, increasing per-record formatting work.

---

## 4. What the Numbers Actually Mean

### 4.1 The benchmark fairness question for slog

The measured 23× improvement from the `PrebufDrain` fix raises an obvious question: is this a
legitimate optimisation, or is it artificially inflating slog's score by giving it capabilities
the library does not actually provide?

The answer requires distinguishing between what the library _can do_ and what the benchmark _asks
it to do_. `slog-json::Json<W>` is generic over any `W: Write`. Nothing in the library prevents
the caller from passing a `BufWriter<File>` instead of a raw `File` — this would reduce the
serde_json streaming writes to buffered in-memory writes with only occasional syscalls. The
library supports it; the original benchmark setup just did not use it.

`PrebufDrain` takes a different approach: it uses slog's `Serializer` trait to perform JSON
serialisation into a `Vec<u8>`, bypassing `slog-json` entirely. The output format changes (no
timestamp, simpler JSON), but it becomes consistent with what env_logger and fern produce in this
benchmark — all three use custom format closures that emit `{"level":"...","msg":"..."}` without
timestamps.

The original slog-json setup was measuring three things simultaneously:

1. The cost of slog's drain chain traversal
2. The cost of `time::OffsetDateTime::now_utc()` + RFC3339 formatting (a timestamp no other logger
   was producing)
3. The cost of serde_json streaming to a raw file

The `PrebufDrain` fix removes (2) and (3), aligning the measurement with what the other loggers
are actually being asked to do. This is not cheating — it is correcting an inconsistency in the
benchmark setup. The remaining cost, (1), is what the benchmark now measures for slog.

### 4.2 The async throughput misconception

A common expectation is that async logging is faster than sync logging because the calling thread
"doesn't wait for the write." This is true for _latency_ — the caller returns quickly. It is not
true for _throughput_ when you measure total wall time including drain.

When you drop an async logger handle and wait for it to finish (as this benchmark does for
`slog_async`, `tracing_async`, and `winston`), all the I/O the worker thread must do is included
in the elapsed time. Moving I/O to a background thread changes _when_ it happens, not _how much_
there is.

The only scenario where async genuinely improves throughput is when:

- The worker can process records faster than the caller generates them (so the worker never
  becomes the bottleneck), AND
- The per-record I/O cost in the worker is lower than it would be in the caller (e.g. because the
  worker can do write coalescing, or because the worker uses a faster path)

For `tracing-appender`, both conditions hold: the worker does 1 syscall per record (not ~10–12
that a naive count of serde_json calls might suggest), and the batch-draining loop in `work()`
means the worker processes many records back-to-back before the OS page cache needs to handle a
flush. The result is genuine throughput improvement (0.328 s async vs 0.649 s sync).

For `slog-async` with the unfixed `slog_json::Json<File>` drain, neither condition holds: the
worker does ~27–30 syscalls per record, and there is no coalescing. The async layer adds overhead
(4–5 allocations per record) without reducing I/O cost. Hence slog-async file is _slower_ than
slog sync file.

### 4.3 Winston's position and what BufWriter would change

Winston sits in an intermediate position. Its file write path issues 2 syscalls per record (vs 1
for the per-record-buffered loggers), and those 2 syscalls are on the worker thread (not the
caller thread). The caller pays an async channel send plus a `format!()` String allocation per
call, which are fast. The worker pays 2 file syscalls per record, which is more than any of the
buffered loggers.

At 100K records: 200K × ~3 µs = ~600 ms of pure file I/O in the worker. The measured elapsed
time of ~925 ms therefore breaks down roughly as ~600 ms file I/O + ~325 ms for channel
overhead, format allocations, `Arc<LogInfo>` creation, and worker processing time.

Adding `BufWriter`:

```rust
WriterTransport::new(BufWriter::new(log_file))
```

reduces the 200K file syscalls to ~770 (100K records × ~60 bytes ÷ 8192 byte BufWriter capacity).
The projected file I/O time drops from ~600 ms to ~2 ms, cutting total elapsed time to roughly
the channel + processing overhead alone. This positions winston alongside fern in the file target
ranking — both would be BufWriter-backed, both would show near-zero I/O overhead relative to
their core work.

The call-site latency profile does not change with BufWriter, because file writes are already
on the worker thread. The P50/P99 numbers seen by the calling thread are determined by the channel
send latency, not the file write latency. What changes is the throughput measurement and the
worker thread's I/O pressure under high load.

---

## 5. References

All sources listed below are Rust crates from crates.io. winston and logform are sourced from
[ifeanyi-ugwu/winston_logger_rs](https://github.com/ifeanyi-ugwu/winston_logger_rs) at commit
`50cdceb8c389fdbdef0aaf0c1acde1ec2a20a1d4`. Crates.io packages are at the standard Cargo registry
location (`~/.cargo/registry/src/index.crates.io-*/`).

| Library            | Version                                                                                                                      | Key source file             | Key lines                                                                            |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------- | --------------------------- | ------------------------------------------------------------------------------------ |
| env_logger         | 0.11.5                                                                                                                       | `src/fmt/mod.rs`            | 131–153 (Formatter struct and write impl), 182–187 (write into Buffer)               |
| fern               | 0.6.2                                                                                                                        | `src/log_impl.rs`           | 99–100 (File struct with Mutex<BufWriter>), 571–599 (format-before-lock in log impl) |
| tracing-subscriber | 0.3.18                                                                                                                       | `src/fmt/fmt_layer.rs`      | 944–991 (on_event with thread_local BUF and single write_all)                        |
| tracing-subscriber | 0.3.18                                                                                                                       | `src/fmt/format/json.rs`    | 219–239 (format_event using serde_json::Serializer into Writer)                      |
| tracing-appender   | 0.2.3                                                                                                                        | `src/non_blocking.rs`       | 244–267 (Write impl: each write_all → one channel send)                              |
| tracing-appender   | 0.2.3                                                                                                                        | `src/worker.rs`             | 30–44 (handle_recv: write_all per message), 56–67 (work: batch drain before flush)   |
| slog-json          | 2.6.1                                                                                                                        | `src/lib.rs`                | 178–183 (Json struct with RefCell<W>), 228–256 (Drain impl streaming to W)           |
| slog-json          | 2.6.1                                                                                                                        | `src/lib.rs`                | 332–346 (add_default_keys: timestamp via time::OffsetDateTime::now_utc)              |
| slog-async         | 2.8.0                                                                                                                        | `lib.rs`                    | 480–505 (AsyncRecord struct and from() constructor with allocations)                 |
| slog-async         | 2.8.0                                                                                                                        | `lib.rs`                    | 453–475 (send and log: one channel send per record)                                  |
| logform            | [winston_logger_rs@50cdceb](https://github.com/ifeanyi-ugwu/winston_logger_rs/tree/50cdceb8c389fdbdef0aaf0c1acde1ec2a20a1d4) | `src/log_info.rs`           | 131–138 (LogInfo Display: writes self.message, no JSON wrapping)                     |
| winston_transport  | [winston_logger_rs@50cdceb](https://github.com/ifeanyi-ugwu/winston_logger_rs/tree/50cdceb8c389fdbdef0aaf0c1acde1ec2a20a1d4) | `src/transport_adapters.rs` | 198–214 (WriterTransport: Mutex<W>, writeln! → 2 write_all per record)               |
| logmark            | `38f03c0` (this repo)                                                                                                        | `src/main.rs`               | PrebufDrain struct and slog::Drain impl                                              |

**serde_json write mechanics** (referenced in §2.5): the `CompactFormatter` implementation in
`serde_json` can be read in `serde_json/src/ser.rs`. Each structural element of a JSON object
(`{`, `,`, key quotes, `:`, value quotes, `}`) maps to a separate `write_all` call on the
underlying `io::Write`. For a three-field object with string values, this produces approximately
27–30 `write_all` calls. The exact count depends on field types (strings require more quote calls
than integers) and the number of fields.

**write() syscall cost figures** cited in §1.1 (2–5 µs on macOS APFS, 1–3 µs on Linux ext4) are
empirical figures consistent with published OS benchmarks and the results observed in this
project. They are not constants: APFS on Apple Silicon with unified memory architecture tends
toward the lower end (2–3 µs), while older spinning disks or network filesystems will be orders
of magnitude higher.
