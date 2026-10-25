# Binary batch WAL validation contract

Before any batch bytes are written, NulangDB validates:

- batch record count is bounded;
- every record targets the same tablet as the durable tail;
- ownership epochs never regress;
- every record names the immediately preceding sequence;
- every sequence is exactly predecessor + 1;
- the complete encoded payload stays within the batch byte limit.

The experimental `BinaryBatchWal` also acquires a persistent exclusive
`<WAL>.lock` sidecar before opening, recovering, or mutating a WAL. It rejects
file/lock symlinks and hardlinks, and canonicalizes parent directories so local
path aliases share one writer lock. The lock stays held until the writer is
dropped or the process exits. This protects a trusted single-host filesystem,
not untrusted directories, NFS locking, or distributed ownership.

For the canonical `FileWal::append_batch` path, direct callers additionally
have an explicit 4,096-record and 64 MiB aggregate encoded-frame budget,
independent of the owner admission policy. Both limits fail before physical
emission; an over-limit error does not poison the handle.

Therefore a semantic validation failure leaves the physical WAL unchanged.

After physical emission starts, any I/O error makes the outcome ambiguous and
the live handle must not be reused. Reopen is the recovery authority, matching
the existing `FileWal` fail-closed philosophy.
