# Nulang Android UI protocol foundation

This Kotlin/JVM module mirrors the **state-machine invariants** of the
renderer-neutral Rust `nulang-ui/1` and `nulang-ui-msg/1` contracts. It is the
first Android integration slice tracked by [universal app issue #146](https://github.com/nulang-org/nulang/issues/146).

## Scope implemented

- typed UI nodes and structured patch operations with copy-on-write publication;
- fail-closed validation of versions, identifiers, event uniqueness, graph
  reachability, cycles, duplicates and multiple parents;
- monotonic full-width UInt64 revisions, accepting a leading `+` but rejecting
  negative values, overflow and non-decimal spellings;
- transactionally applied ordered patches and atomic snapshot publication;
- iterative validation of deeply nested trees without JVM recursion;
- typed values retaining integer, float-bit and byte identity.

**Implemented in the stacked JSON-codec slice:** bounded, duplicate-key-rejecting
runtime-to-host `nulang-ui-msg/1` JSON snapshot and patch decoding. Tagged
`i64`, raw `f64` bits, unsigned revisions, and byte arrays are decoded
without floating-point coercion. The decoded document/patch is validated before
publication through `UiRuntimeState.applyJson`. JSON is limited to 8 MiB, with
a 64-level nested-value limit and a 1 MiB individual string limit.

**Not yet implemented:** cross-language Rust/Kotlin fixture roundtrips,
host-to-runtime action encoding, the Android AAR, native Nulang `.nbc` artifact loading,
client action authorization, Compose rendering, navigation, offline storage,
or app background execution. The strict decoder handles only inbound
runtime-to-host snapshots and patches; it does not authenticate the sender,
authorize actions or negotiate capabilities. Do not treat parsed objects as
transport-level authority. Cross-language fixture parity remains a release gate.

## Verification

Run the dedicated executable conformance task:

```sh
gradle -p platforms/android/NulangUIProtocol check
```

To execute without Gradle using an installed Kotlin/JVM compiler:

```sh
kotlinc \
  platforms/android/NulangUIProtocol/src/main/kotlin/org/nulang/ui/Protocol.kt \
  platforms/android/NulangUIProtocol/src/test/kotlin/org/nulang/ui/ProtocolConformanceTests.kt \
  -include-runtime -d /tmp/nulang-ui-protocol-tests.jar
java -jar /tmp/nulang-ui-protocol-tests.jar
```

Passing both suites proves the Kotlin reference state machine and validated JSON ingress only.
It does **not** certify Android devices, Java/Kotlin packaging, Nulang bytecode,
or SwiftUI/Compose rendering parity.
