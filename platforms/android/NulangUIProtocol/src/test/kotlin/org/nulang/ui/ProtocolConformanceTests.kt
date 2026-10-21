package org.nulang.ui

private fun mustFail(label: String, block: () -> Unit) {
    val failed = try {
        block()
        false
    } catch (_: UiProtocolException) {
        true
    }
    check(failed) { "$label must fail closed" }
}

private fun node(id: String, kind: String = "text") = UiNode(id, kind)
private fun initialDocument() = UiDocument(
    documentId = "tasks",
    revision = Revision(1uL),
    root = "root",
    nodes = listOf(
        UiNode("root", "column", children = listOf("button")),
        node("button", "button")
    )
)
private fun binding(event: String, id: String) = ActionBinding(event, id, ActionPlacement.CLIENT)

private fun test_duplicate_action_bindings_fail() {
    val bad = initialDocument().copy(nodes = listOf(
        UiNode("root", "column", children = listOf("button")),
        UiNode("button", "button", actions = listOf(binding("press", "one"), binding("press", "two")))
    ))
    mustFail("duplicate event on same node") { bad.validate() }
    val valid = bad.copy(nodes = bad.nodes.map { n ->
        if (n.id == "button") n.copy(actions = listOf(binding("press", "one"), binding("tap", "two"))) else n
    })
    valid.validate()
}

private fun test_revision_wire_range() {
    check(Revision.parse("18446744073709551615") == Revision(ULong.MAX_VALUE))
    check(Revision.parse("+42") == Revision(42uL))
    for (bad in listOf("-0", "-1", "1.0", "", "18446744073709551616")) {
        mustFail("invalid unsigned revision $bad") { Revision.parse(bad) }
    }
}

private fun test_patch_atomicity_and_replay() {
    val state = UiRuntimeState()
    state.apply(RuntimeMessage.Snapshot(initialDocument()))
    val before = state.document
    val invalidPatch = UiPatch(
        documentId = "tasks", baseRevision = Revision(1uL), revision = Revision(2uL),
        operations = listOf(PatchOperation.ReplaceActions("button", listOf(
            binding("press", "one"), binding("press", "two")
        )))
    )
    mustFail("ambiguous patch") { state.apply(RuntimeMessage.Patch(invalidPatch)) }
    check(state.document == before) { "failed patch must preserve complete state" }
    val patch = invalidPatch.copy(operations = listOf(
        PatchOperation.ReplaceActions("button", listOf(binding("press", "one"))),
        PatchOperation.SetProperty("button", "label", WireValue.Text("Save"))
    ))
    state.apply(RuntimeMessage.Patch(patch))
    check(state.document?.revision == Revision(2uL))
    check(state.document?.nodes?.first { it.id == "button" }?.properties?.get("label") == WireValue.Text("Save"))
    mustFail("revision replay") { state.apply(RuntimeMessage.Patch(patch)) }
}

private fun test_invalid_snapshot_preserves_last_valid_document() {
    val state = UiRuntimeState()
    state.apply(RuntimeMessage.Snapshot(initialDocument()))
    val before = state.document
    mustFail("orphan snapshot") {
        state.apply(RuntimeMessage.Snapshot(initialDocument().copy(nodes = initialDocument().nodes + node("orphan"))))
    }
    check(state.document == before)
    mustFail("patch with missing child") {
        state.apply(RuntimeMessage.Patch(UiPatch("tasks", Revision(1uL), Revision(2uL), listOf(
            PatchOperation.ReplaceChildren("root", listOf("ghost"))
        ))))
    }
    check(state.document == before)
}

private fun test_cycle_and_nonincreasing_revision() {
    mustFail("cycle") {
        UiDocument("cycle", Revision(1uL), "root", listOf(
            UiNode("root", "column", children = listOf("child")),
            UiNode("child", "column", children = listOf("root"))
        )).validate()
    }
    mustFail("non-increasing patch") { UiPatch("tasks", Revision(7uL), Revision(7uL), emptyList()).validate() }
}

private fun test_deep_tree_does_not_recurse() {
    val depth = 20_000
    val nodes = (0 until depth).map { i ->
        UiNode("node-$i", "column", children = if (i + 1 == depth) emptyList() else listOf("node-${i + 1}"))
    }
    UiDocument("deep", Revision(1uL), "node-0", nodes).validate()
}

private fun test_missing_snapshot_rejected() {
    val state = UiRuntimeState()
    mustFail("patch before initial snapshot") {
        state.apply(RuntimeMessage.Patch(UiPatch("tasks", Revision(1uL), Revision(2uL), emptyList())))
    }
    check(state.document == null)
}

private fun test_version_and_identity_boundaries() {
    mustFail("unsupported snapshot envelope") {
        UiRuntimeState().apply(RuntimeMessage.Snapshot(initialDocument(), protocol = "nulang-ui-msg/99"))
    }
    mustFail("unsupported document protocol") { initialDocument().copy(protocol = "nulang-ui/99").validate() }
    mustFail("blank action id") {
        initialDocument().copy(nodes = listOf(
            UiNode("root", "column", children = listOf("button")),
            UiNode("button", "button", actions = listOf(binding("press", " ")))
        )).validate()
    }
    mustFail("duplicate node") {
        initialDocument().copy(nodes = initialDocument().nodes + node("button")).validate()
    }
}

private fun test_multiple_parents_and_duplicate_child_rejected() {
    mustFail("duplicate child") {
        initialDocument().copy(nodes = listOf(
            UiNode("root", "column", children = listOf("button", "button")), node("button")
        )).validate()
    }
    mustFail("multiple parents") {
        UiDocument("multi", Revision(1uL), "root", listOf(
            UiNode("root", "column", children = listOf("a", "b")),
            UiNode("a", "column", children = listOf("shared")),
            UiNode("b", "column", children = listOf("shared")),
            node("shared")
        )).validate()
    }
}

private fun test_patch_document_identity_and_order_enforced() {
    val state = UiRuntimeState()
    state.apply(RuntimeMessage.Snapshot(initialDocument()))
    mustFail("different document id") {
        state.apply(RuntimeMessage.Patch(UiPatch("other", Revision(1uL), Revision(2uL), emptyList())))
    }
    mustFail("out-of-order patch") {
        state.apply(RuntimeMessage.Patch(UiPatch("tasks", Revision(0uL), Revision(2uL), emptyList())))
    }
    mustFail("remove missing node") {
        state.apply(RuntimeMessage.Patch(UiPatch("tasks", Revision(1uL), Revision(2uL), listOf(
            PatchOperation.RemoveNode("missing")
        ))))
    }
    check(state.document == initialDocument())
}

fun main() {
    test_duplicate_action_bindings_fail()
    test_revision_wire_range()
    test_patch_atomicity_and_replay()
    test_invalid_snapshot_preserves_last_valid_document()
    test_cycle_and_nonincreasing_revision()
    test_deep_tree_does_not_recurse()
    test_missing_snapshot_rejected()
    test_version_and_identity_boundaries()
    test_multiple_parents_and_duplicate_child_rejected()
    test_patch_document_identity_and_order_enforced()
    println("Nulang Kotlin UI protocol conformance: 10/10 passed")
}
