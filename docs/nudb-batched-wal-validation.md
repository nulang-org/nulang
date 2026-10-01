# Binary batch WAL validation contract

Before any batch bytes are written, NulangDB validates:

- batch record count is bounded;
- every record targets the same tablet as the durable tail;
- ownership epochs never regress;
- every record names the immediately preceding sequence;
- every sequence is exactly predecessor + 1;
- the complete encoded payload stays within the batch byte limit.

Therefore a semantic validation failure leaves the physical WAL unchanged.

After physical emission starts, any I/O error makes the outcome ambiguous and
the live handle must not be reused. Reopen is the recovery authority, matching
the existing `FileWal` fail-closed philosophy.
