# NulangDB tablet-owner group commit coalescing

This stacked storage slice keeps the tablet owner as the admission and ownership state machine while preserving ordinary local Rust for the WAL/MVCC hot path.

The default admission policy is bounded to 256 writes, approximately 1 MiB of encoded write data, and 100 microseconds of queue age. The owner flushes earlier when the next FIFO write cannot fit under the byte target, and an oversized queue head is always admitted so the byte target cannot starve progress. Drain/handoff bypasses the coalescing delay.

A coalesced group is a durability optimization, not an atomic transaction. Request-level acknowledgements are returned only after `WalBackedTablet::commit_batch` reports success, but an interrupted or failed group may leave a durable physical prefix. Any ambiguous storage failure therefore transitions the owner to `Faulted`, invalidates every unacknowledged request in the failed group and remaining queue, and requires reopen/recovery before retry decisions are made.

The runtime or actor driver is responsible for scheduling the coalescing deadline. The storage core exposes `process_batch_if_due` and does not sleep internally.

Tests cover FIFO admission, capacity backpressure, ownership epoch fencing, draining/handoff behavior, write and byte batch limits, oversized-head fairness, exact low-load delay boundaries, immediate byte-boundary and drain flushes, and ambiguous group failure followed by authoritative-prefix recovery.
