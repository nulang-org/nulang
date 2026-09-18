# Nulang Fabric Streams

Fabric Streams are the durable-log layer above Fabric's ephemeral topic routing.
This first implementation deliberately establishes the local storage contract
before adding partition replication or broker-compatibility semantics.

## Implemented storage contract

`FileFabricStreamStore` provides an append-only segmented log rooted at a caller
selected directory.

Each stream has:

- `meta.json` — format version and segment-size configuration.
- `<base-sequence>.seg` — ordered immutable-history segments.
- `cursors.json` — atomically replaced consumer cursor state.

Records are assigned monotonically increasing 64-bit sequence numbers starting
at 1. A record frame contains:

1. sequence number,
2. payload length,
3. a 128-bit truncated BLAKE3 checksum over sequence + payload,
4. opaque payload bytes.

The payload is intentionally opaque at this layer. Future typed `stream`
declarations can choose their serialization without changing the durable-log
format.

### Durability

A successful append writes the complete frame, flushes it, and calls
`sync_data` before returning its sequence number.

Metadata and cursor updates use write-to-temp + `sync_all` + atomic rename.
The containing directory is synchronized after replacement.

### Crash recovery

Opening a store is cheap; an individual stream is recovered lazily on first
access. Recovery scans segments in base-sequence order and verifies:

- segment magic and format version,
- filename/header base-sequence agreement,
- sequence continuity across every record and segment,
- per-record checksum integrity.

An incomplete final frame is treated as a torn tail and truncated to the last
complete record. Sequence gaps or checksum mismatches fail closed rather than
silently discarding committed history.

### Segment rotation

`FabricStreamConfig::segment_max_bytes` controls rotation. Rotation happens
before appending a frame that would exceed the configured size, except that one
record larger than the target is allowed to occupy its own segment.

### Consumer cursors and replay

A cursor is the last fully processed stream sequence for a named consumer.
Cursor commits are monotonic and cannot advance beyond the current stream tail.

`read_consumer(stream, consumer, limit)` replays from `cursor + 1`.

This establishes the persistence primitive needed for later ACK/NACK semantics
without pretending that ACK redelivery already exists.

## Runtime APIs

After opening storage:

```rust
runtime.fabric_stream_open("/var/lib/nulang/fabric")?;
runtime.fabric_stream_create("orders", FabricStreamConfig::default())?;
let sequence = runtime.fabric_stream_append("orders", payload)?;

let records = runtime.fabric_stream_read("orders", 1, 100)?;
runtime.fabric_stream_commit_cursor("orders", "billing", sequence)?;
let pending = runtime.fabric_stream_read_consumer("orders", "billing", 100)?;
```

Available APIs:

- `fabric_stream_open`
- `fabric_stream_create`
- `fabric_stream_append`
- `fabric_stream_read`
- `fabric_stream_read_consumer`
- `fabric_stream_commit_cursor`
- `fabric_stream_cursor`
- `fabric_stream_info`

## Explicitly not implemented yet

This storage PR does **not** claim distributed stream semantics. Follow-up work
must add:

1. partition ownership and replica placement,
2. replicated append / quorum policy,
3. ACK/NACK and timed redelivery,
4. retention by age/bytes/sequence,
5. dead-letter streams,
6. producer deduplication/idempotency keys,
7. consumer groups and partition assignment,
8. sequence/time seek indexes,
9. typed language-level stream declarations,
10. NATS JetStream compatibility only after native semantics stabilize.

The architectural boundary is intentional: the append log should remain usable
for embedded/single-node Nulang even when cluster replication is disabled.
