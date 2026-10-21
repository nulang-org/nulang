package org.nulang.ui

/** Pure-JVM wire regressions. These must run explicitly: no zero-test JUnit pass. */
private fun reject(label: String, json: String) {
    val failed = try {
        NulangUiJsonCodec.decodeRuntimeMessage(json)
        false
    } catch (_: UiProtocolException) {
        true
    }
    check(failed) { label + " must be rejected" }
}

private fun snapshot(): String = """
{"type":"snapshot","protocol":"nulang-ui-msg/1",
 "document":{"protocol":"nulang-ui/1","document_id":"tasks",
 "revision":"18446744073709551613","root":"root","nodes":[
 {"id":"root","kind":"column","children":["button"]},
 {"id":"button","kind":"button","properties":{
 "count":{"type":"i64","value":"-9223372036854775808"},
 "bits":{"type":"f64","value":"7ff8000000000042"},
 "raw":{"type":"bytes","value":[0,1,127,128,255]},
 "none":{"type":"null"},"title":{"type":"string","value":"héllo"}}}]}}
""".trimIndent()

private fun test_lossless_snapshot_and_ordered_patch() {
    val state = UiRuntimeState()
    state.applyJson(snapshot())
    val old = checkNotNull(state.document)
    check(old.revision == Revision(ULong.MAX_VALUE - 2u))
    val node = old.nodes.first { it.id == "button" }
    check(node.properties["count"] == WireValue.SignedInt(Long.MIN_VALUE))
    check(node.properties["bits"] == WireValue.FloatBits(0x7ff8000000000042uL))
    check(node.properties["raw"] == WireValue.Bytes(listOf(0u,1u,127u,128u,255u).map { it.toUByte() }))
    check(node.properties["none"] == WireValue.Null)

    val patch = """
{"type":"patch","protocol":"nulang-ui-msg/1","patch":{"protocol":"nulang-ui/1",
 "document_id":"tasks","base_revision":"18446744073709551613","revision":"18446744073709551614",
 "operations":[
  {"op":"set_property","node_id":"button","name":"title","value":{"type":"string","value":"saved"}},
  {"op":"replace_actions","node_id":"button","actions":[
   {"event":"press","action_id":"task.save","placement":"client"}]}
 ]}}
""".trimIndent()
    state.applyJson(patch)
    check(state.document?.revision == Revision(ULong.MAX_VALUE - 1u))
    check(state.document?.nodes?.first { it.id == "button" }?.properties?.get("title") == WireValue.Text("saved"))
    val after = state.document
    check(node.properties["title"] == WireValue.Text("héllo"))
    try { state.applyJson(patch); error("replay accepted") }
    catch (_: UiProtocolException) { check(state.document == after) }
}

private fun test_reject_malformed_envelopes_and_semantic_documents() {
    val good = """{"type":"snapshot","protocol":"nulang-ui-msg/1",
    "document":{"protocol":"nulang-ui/1","document_id":"tasks","revision":"1",
    "root":"root","nodes":[{"id":"root","kind":"text"}]}}"""
    reject("duplicate key", good.replace("\"type\":\"snapshot\"", "\"type\":\"snapshot\",\"type\":\"snapshot\""))
    reject("wrong message version", good.replace("nulang-ui-msg/1","nulang-ui-msg/2"))
    reject("wrong document version", good.replace("nulang-ui/1","nulang-ui/2"))
    reject("negative zero revision", good.replace("\"revision\":\"1\"","\"revision\":\"-0\""))
    reject("numeric revision", good.replace("\"revision\":\"1\"","\"revision\":1"))
    reject("explicit null actions", good.replace("\"kind\":\"text\"", "\"kind\":\"text\",\"actions\":null"))
    reject("orphan", good.replace("\"kind\":\"text\"}]","\"kind\":\"text\"},{\"id\":\"orphan\",\"kind\":\"text\"}]"))
    reject("duplicate event", good.replace("\"kind\":\"text\"", """ "kind":"text","actions":[
        {"event":"press","action_id":"a","placement":"client"},
        {"event":"press","action_id":"b","placement":"client"}] """))
    reject("nonincreasing patch", """{"type":"patch","protocol":"nulang-ui-msg/1",
      "patch":{"protocol":"nulang-ui/1","document_id":"tasks","base_revision":"9","revision":"9","operations":[]}}""")
}

private fun test_reject_lossy_values_and_nonjson_suffix() {
    val original = snapshot()
    reject("i64 as number", original.replace("\"value\":\"-9223372036854775808\"", "\"value\":-9223372036854775808"))
    reject("overflow i64", original.replace("-9223372036854775808", "9223372036854775808"))
    reject("bad floating bits", original.replace("7ff8000000000042","bad"))
    reject("invalid byte", original.replace("[0,1,127,128,255]","[0,256]"))
    reject("unknown tag", original.replace("\"type\":\"bytes\"", "\"type\":\"uint64\""))
    reject("extra JSON", original + "{}")
    reject("truncated JSON", original.dropLast(10))
    reject("oversize JSON", " ".repeat(8 * 1024 * 1024 + 1))
}

private fun test_invalid_patch_does_not_mutate_state() {
    val state = UiRuntimeState()
    state.applyJson(snapshot())
    val original = state.document
    val invalid = """{"type":"patch","protocol":"nulang-ui-msg/1",
      "patch":{"protocol":"nulang-ui/1","document_id":"tasks",
      "base_revision":"18446744073709551613","revision":"18446744073709551614",
      "operations":[{"op":"remove_node","node_id":"root"}]}}"""
    try { state.applyJson(invalid); error("orphaned tree accepted") }
    catch (_: UiProtocolException) { check(state.document == original) }
}

fun main() {
    test_lossless_snapshot_and_ordered_patch()
    test_reject_malformed_envelopes_and_semantic_documents()
    test_reject_lossy_values_and_nonjson_suffix()
    test_invalid_patch_does_not_mutate_state()
    println("Nulang Kotlin UI JSON codec conformance: 4/4 suites passed")
}
