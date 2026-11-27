# NuDB frozen format fixtures

These are independently constructed specification vectors for the NUDBWAL3
and NUDBCKP1 formats as implemented at
`1f247db23d94c6f711441c3e737e2c5dd974afb3`. They are not archived production
databases or captures from a historical released binary. The legacy checkpoint
vector models the pre-coalescing duplicate sequence representation. The
integration test verifies the current writer byte-for-byte against the canonical
vectors and opens disposable copies rather than the original fixture files.

All integer framing fields are little-endian. WAL header and frame header
prefixes and JSON record payloads have separate BLAKE3 digests; checkpoints
have a BLAKE3 payload digest. WAL frame, record and checkpoint payload version
are all 1. The tablet ID is 987, epoch 7, range [a,z). Transactions:

1. Put k = bytes 00 ff, m = empty bytes.
2. Put k = 01, delete k, put k = "last", delete m.
3. Delete k, put n = "tail".

The full WAL has base 0; the reclaimed WAL has base 2 and transaction 3.
The canonical checkpoint holds sequence 2; the legacy checkpoint additionally
contains repeated sequence-2 mutations for k in original order.

## Immutable fixture policy

Run `cargo test --locked --no-default-features --test nudb_format_golden`.
Never regenerate these files as a normal test or update them merely to make a
changed reader pass. A deliberately versioned format change must retain old
read vectors, add new vectors and explicitly document writer compatibility.
A writer-byte mismatch is a review trigger, not evidence that every different
JSON encoding is corrupt. These vectors do not certify power-loss durability,
all historical formats, or distributed compatibility.

SHA-256 (audit only; the database uses BLAKE3):
b0f63bb4eb759752ffeff154be1eb37e59018a0b747783fc5991fc15f29fe166  ckp1-canonical.bin
8318fdb1c90d99b99af3796b4ad624b473cb6f9214b0daf425602afd929568c8  ckp1-legacy-duplicates.bin
736a60a95eb54e16156cf417a463188e8d4b23bed61570d1f2a947f588a2ecfb  wal3-full.bin
35f6a2764ec0b7f534fc3fdbf78e39f7b8939e8b7b1c4f83319f5a6ea1aae7a2  wal3-reclaimed.bin
