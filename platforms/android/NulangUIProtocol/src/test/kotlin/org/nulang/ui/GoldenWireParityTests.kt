package org.nulang.ui

import java.nio.file.Files
import java.nio.file.Path

/**
 * Shared immutable v1 wire fixtures consumed by the Rust protocol crate.
 * Run as an executable suite so "zero discovered JUnit tests" cannot pass CI.
 */
private fun fixture(name: String): String {
    val path = Path.of("../../../tests/fixtures/ui_protocol/v1/$name.json")
    check(Files.isRegularFile(path)) { "missing shared wire fixture: $path" }
    return Files.readString(path)
}

private fun test_shared_valid_snapshot_preserves_lossless_values() {
    val message = NulangUiJsonCodec.decodeRuntimeMessage(fixture("snapshot_valid"))
    check(message is RuntimeMessage.Snapshot)
    val doc = message.document
    check(doc.revision == Revision(ULong.MAX_VALUE - 2uL))
    val button = doc.nodes.first { it.id == "button" }
    check(button.actions.map { it.event } == listOf("press", "focus"))
    check(button.properties["count"] == WireValue.SignedInt(Long.MIN_VALUE))
    check(button.properties["top"] == WireValue.SignedInt(Long.MAX_VALUE))
    check(button.properties["bits"] == WireValue.FloatBits(0x7ff8000000000042uL))
    check(button.properties["raw"] ==
        WireValue.Bytes(listOf(0, 1, 127, 128, 255).map { it.toUByte() }))
    check(button.properties["title"] == WireValue.Text("héllo 🌍"))
    check(button.properties["nested"] == WireValue.Array(listOf(
        WireValue.Null,
        WireValue.Object(mapOf("saved" to WireValue.Bool(true)))
    )))
}

private fun test_shared_valid_patch_and_replay_atomicity() {
    val state = UiRuntimeState()
    state.applyJson(fixture("snapshot_valid"))
    state.applyJson(fixture("patch_valid"))
    check(state.document?.revision == Revision(ULong.MAX_VALUE - 1uL))
    val button = state.document!!.nodes.first { it.id == "button" }
    check(button.properties["title"] == WireValue.Text("saved"))
    check(button.actions.map { it.actionId } == listOf("save", "audit"))
    val before = state.document
    try {
        state.applyJson(fixture("patch_valid"))
        error("replayed patch was accepted")
    } catch (_: UiProtocolException) {
        check(state.document == before) { "replay mutated the document" }
    }
}

private fun test_shared_invalid_fixtures_are_rejected() {
    for (name in listOf(
        "snapshot_duplicate_event",
        "snapshot_null_actions",
        "snapshot_negative_zero",
        "patch_nonincreasing",
        "snapshot_duplicate_key",
        "snapshot_invalid_float",
    )) {
        val state = UiRuntimeState()
        val previous = state.document
        try {
            state.applyJson(fixture(name))
            error("Kotlin unexpectedly accepted $name")
        } catch (_: UiProtocolException) {
            check(state.document == previous) { "$name mutated state" }
        }
    }
}

fun main() {
    test_shared_valid_snapshot_preserves_lossless_values()
    test_shared_valid_patch_and_replay_atomicity()
    test_shared_invalid_fixtures_are_rejected()
    println("Nulang Rust/Kotlin shared wire v1: 8/8 fixtures passed")
}
