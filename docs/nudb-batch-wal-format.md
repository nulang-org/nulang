# Binary batch WAL v1

The experimental batch WAL is intentionally separate from `NUDBWAL3` until it
has equivalent crash/fault coverage.

## File

```text
file := "NUDBBW01" frame*
```

## Frame

```text
frame :=
  magic:"NBAT"
  version:u16
  record_count:u32
  payload_len:u32
  header_blake3:[u8;32]
  payload:[u8;payload_len]
  payload_blake3:[u8;32]
```

The header checksum covers the fixed prefix through `payload_len`. The payload
checksum covers the complete encoded batch.

## Record payload

```text
record :=
  tablet_id:u64
  ownership_epoch:u64
  sequence:u64
  expected_previous_sequence:u64
  mutation_count:u32
  mutation*

put := 1:u8 key_len:u32 key value_len:u32 value
delete := 2:u8 key_len:u32 key
```

All integer fields are little-endian.

## Commit boundary

A batch is completely validated and encoded before the first file mutation.
After emission starts the live handle is considered poisoned until the complete
frame has passed `sync_data`. Successful batches update in-memory tail metadata
only after that durability boundary.

The important performance property is one durability synchronization for many
logical writes while retaining explicit sequence and ownership fencing.
