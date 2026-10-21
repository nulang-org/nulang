# Renderer-neutral `nulang-ui/1` golden wire fixtures

These JSON documents are a shared acceptance/rejection contract for the **Rust**
`nulang-ui-protocol` crate and the **Kotlin/JVM** `NulangUIProtocol` host.

| File | Expected | Purpose |
| --- | --- | --- |
| `snapshot_valid.json` | Accept | Full-width unsigned revision, signed integer extremes, exact NaN bit payload, byte array, nested tagged data, Unicode, distinct bound actions |
| `patch_valid.json` | Accept after snapshot | Ordered property/action mutation and replay rejection |
| `snapshot_duplicate_event.json` | Reject | Ambiguous action event binding on one node |
| `snapshot_null_actions.json` | Reject | Explicit null does not equal omitted actions |
| `snapshot_negative_zero.json` | Reject | Negative-zero unsigned revision spelling |
| `patch_nonincreasing.json` | Reject | Revision must strictly increase |
| `snapshot_duplicate_key.json` | Reject | Duplicate JSON envelope discriminator |
| `snapshot_invalid_float.json` | Reject | `f64` must have exactly sixteen hexadecimal digits |

Both languages must consume these **unchanged files**, not independent inline
copies, and agree on the outcome. Tests assert semantic state and preserved
values rather than noncanonical raw JSON formatting.

**Commands** (from the repository root):

```sh
cargo test -p nulang-ui-protocol --test golden_wire_parity --locked
gradle -p platforms/android/NulangUIProtocol goldenWireConformance --no-daemon
```

Rust test file: `crates/nulang-ui-protocol/tests/golden_wire_parity.rs`.
Kotlin test file: `platforms/android/NulangUIProtocol/src/test/kotlin/org/nulang/ui/GoldenWireParityTests.kt`.

This validates the runtime-to-host snapshot/patch subset only. It does not
certify native JNI/AAR packaging, Compose rendering, SwiftUI parity, sender
authentication, host action authorization, or byte-for-byte re-encoding.
Extending the protocol must add or version shared fixtures instead of silently
changing v1 expectations.
