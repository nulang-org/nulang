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

**Not yet implemented:** an untrusted JSON codec, exact cross-language wire
fixture roundtrips, the Android AAR, native Nulang `.nbc` artifact loading,
client action authorization, Compose rendering, navigation, offline storage,
or app background execution. Do not expose incoming network or native messages
through this model until a strict JSON codec and canonical Rust/Kotlin fixture
parity are integrated. Kotlin object models are not transport-level authority.

## Verification

Run the dedicated executable conformance task:

```sh
gradle -p platforms/android/NulangUIProtocol protocolConformance
```

To execute without Gradle using an installed Kotlin/JVM compiler:

```sh
kotlinc \
  platforms/android/NulangUIProtocol/src/main/kotlin/org/nulang/ui/Protocol.kt \
  platforms/android/NulangUIProtocol/src/test/kotlin/org/nulang/ui/ProtocolConformanceTests.kt \
  -include-runtime -d /tmp/nulang-ui-protocol-tests.jar
java -jar /tmp/nulang-ui-protocol-tests.jar
```

Passing this suite proves the Kotlin reference state machine only.
It does **not** certify Android devices, Java/Kotlin packaging, Nulang bytecode,
or SwiftUI/Compose rendering parity.
