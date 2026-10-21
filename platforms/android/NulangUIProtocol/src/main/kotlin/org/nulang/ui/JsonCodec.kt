package org.nulang.ui

import com.fasterxml.jackson.core.JsonFactory
import com.fasterxml.jackson.core.JsonParser
import com.fasterxml.jackson.core.JsonProcessingException
import com.fasterxml.jackson.core.JsonToken
import com.fasterxml.jackson.core.StreamReadConstraints
import com.fasterxml.jackson.core.StreamReadFeature

/**
 * Strict, bounded runtime-to-host JSON ingress for nulang-ui-msg/1.
 * This does not authorize client actions or execute any Nulang code.
 */
object NulangUiJsonCodec {
    private const val MAX_DOCUMENT_BYTES = 8 * 1024 * 1024
    private const val MAX_NESTING = 64

    private val factory = JsonFactory.builder()
        .enable(StreamReadFeature.STRICT_DUPLICATE_DETECTION)
        .streamReadConstraints(
            StreamReadConstraints.builder()
                .maxNestingDepth(MAX_NESTING)
                .maxStringLength(1_048_576)
                .maxNumberLength(64)
                .build()
        ).build()

    fun decodeRuntimeMessage(json: String): RuntimeMessage {
        if (json.length > MAX_DOCUMENT_BYTES ||
            json.toByteArray(Charsets.UTF_8).size > MAX_DOCUMENT_BYTES) {
            fail("UI message exceeds byte limit")
        }
        val root = try {
            factory.createParser(json).use { parser ->
                val value = readValue(parser, 0)
                if (parser.nextToken() != null) fail("trailing JSON tokens")
                value
            }
        } catch (e: JsonProcessingException) {
            throw UiProtocolException("invalid UI JSON: " + e.originalMessage)
        }

        val obj = root.obj("message")
        val protocol = obj.text("protocol")
        if (protocol != UiProtocol.MESSAGE_VERSION) fail("unsupported message protocol")
        return when (obj.text("type")) {
            "snapshot" -> RuntimeMessage.Snapshot(document(obj.field("document"))).also {
                it.document.validate()
            }
            "patch" -> RuntimeMessage.Patch(patch(obj.field("patch"))).also {
                it.patch.validate()
            }
            else -> fail("unknown runtime message type")
        }
    }

    private fun document(value: J): UiDocument {
        val obj = value.obj("document")
        return UiDocument(
            protocol = obj.text("protocol"),
            documentId = obj.text("document_id"),
            revision = Revision.parse(obj.text("revision")),
            root = obj.text("root"),
            nodes = obj.field("nodes").arr("nodes").map(::node)
        )
    }

    private fun node(value: J): UiNode {
        val obj = value.obj("node")
        val properties = obj.optional("properties")?.obj("properties")
            ?.mapValues { (_, v) -> wire(v) } ?: emptyMap()
        val children = obj.optional("children")?.arr("children")
            ?.map { it.str("child") } ?: emptyList()
        val actions = obj.optional("actions")?.arr("actions")
            ?.map(::binding) ?: emptyList()
        return UiNode(
            id = obj.text("id"), kind = obj.text("kind"),
            properties = properties, children = children, actions = actions
        )
    }

    private fun binding(value: J): ActionBinding {
        val obj = value.obj("action binding")
        val placement = when (obj.text("placement")) {
            "client" -> ActionPlacement.CLIENT
            "server" -> ActionPlacement.SERVER
            else -> fail("unknown action placement")
        }
        return ActionBinding(obj.text("event"), obj.text("action_id"), placement)
    }

    private fun patch(value: J): UiPatch {
        val obj = value.obj("patch")
        return UiPatch(
            protocol = obj.text("protocol"),
            documentId = obj.text("document_id"),
            baseRevision = Revision.parse(obj.text("base_revision")),
            revision = Revision.parse(obj.text("revision")),
            operations = obj.field("operations").arr("operations").map(::operation)
        )
    }

    private fun operation(value: J): PatchOperation {
        val obj = value.obj("patch operation")
        return when (obj.text("op")) {
            "upsert_node" -> PatchOperation.UpsertNode(node(obj.field("node")))
            "remove_node" -> PatchOperation.RemoveNode(obj.text("node_id"))
            "set_root" -> PatchOperation.SetRoot(obj.text("node_id"))
            "set_property" -> PatchOperation.SetProperty(
                obj.text("node_id"), obj.text("name"), wire(obj.field("value")))
            "remove_property" -> PatchOperation.RemoveProperty(
                obj.text("node_id"), obj.text("name"))
            "replace_children" -> PatchOperation.ReplaceChildren(
                obj.text("node_id"), obj.field("children").arr("children").map { it.str("child") })
            "replace_actions" -> PatchOperation.ReplaceActions(
                obj.text("node_id"), obj.field("actions").arr("actions").map(::binding))
            else -> fail("unknown patch operation")
        }
    }

    private fun wire(value: J): WireValue {
        val obj = value.obj("wire value")
        return when (obj.text("type")) {
            "null" -> WireValue.Null
            "bool" -> WireValue.Bool(
                (obj.field("value") as? J.Bool)?.value ?: fail("bool requires boolean"))
            "i64" -> WireValue.SignedInt(
                obj.text("value").toLongOrNull() ?: fail("i64 is outside signed 64-bit range"))
            "f64" -> {
                val hex = obj.text("value")
                if (hex.length != 16 || !hex.all {
                    it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F'
                }) fail("f64 must contain 16 hexadecimal digits")
                WireValue.FloatBits(hex.toULong(16))
            }
            "string" -> WireValue.Text(obj.text("value"))
            "bytes" -> WireValue.Bytes(obj.field("value").arr("bytes").map { element ->
                val raw = (element as? J.Number)?.raw ?: fail("byte must be an integer")
                val n = raw.toIntOrNull() ?: fail("invalid byte")
                if (n !in 0..255) fail("byte outside 0..255")
                n.toUByte()
            })
            "array" -> WireValue.Array(obj.field("value").arr("array").map(::wire))
            "object" -> WireValue.Object(obj.field("value").obj("object")
                .mapValues { (_, element) -> wire(element) })
            else -> fail("unknown wire value type")
        }
    }

    /** Numeric tokens retain source text and never travel through floating point. */
    private sealed interface J {
        data class Obj(val values: Map<String, J>) : J
        data class Arr(val values: List<J>) : J
        data class Str(val value: String) : J
        data class Number(val raw: String) : J
        data class Bool(val value: Boolean) : J
        data object Null : J
    }

    private fun readValue(parser: JsonParser, depth: Int): J {
        if (parser.nextToken() == null) fail("incomplete JSON")
        return readCurrentValue(parser, depth)
    }

    private fun readCurrentValue(parser: JsonParser, depth: Int): J {
        if (depth > MAX_NESTING) fail("JSON nesting limit exceeded")
        return when (parser.currentToken) {
            JsonToken.START_OBJECT -> {
                val fields = LinkedHashMap<String, J>()
                while (parser.nextToken() != JsonToken.END_OBJECT) {
                    if (parser.currentToken != JsonToken.FIELD_NAME) fail("expected object key")
                    val name = parser.currentName
                    if (fields.containsKey(name)) fail("duplicate JSON key")
                    fields[name] = readValue(parser, depth + 1)
                }
                J.Obj(fields)
            }
            JsonToken.START_ARRAY -> {
                val elements = ArrayList<J>()
                while (parser.nextToken() != JsonToken.END_ARRAY) {
                    if (parser.currentToken == null) fail("unterminated array")
                    elements.add(readCurrentValue(parser, depth + 1))
                }
                J.Arr(elements)
            }
            JsonToken.VALUE_STRING -> J.Str(parser.text)
            JsonToken.VALUE_NUMBER_INT, JsonToken.VALUE_NUMBER_FLOAT -> J.Number(parser.text)
            JsonToken.VALUE_TRUE -> J.Bool(true)
            JsonToken.VALUE_FALSE -> J.Bool(false)
            JsonToken.VALUE_NULL -> J.Null
            else -> fail("invalid JSON value")
        }
    }

    private fun J.obj(label: String): Map<String, J> =
        (this as? J.Obj)?.values ?: fail(label + " must be an object")
    private fun J.arr(label: String): List<J> =
        (this as? J.Arr)?.values ?: fail(label + " must be an array")
    private fun J.str(label: String): String =
        (this as? J.Str)?.value ?: fail(label + " must be a string")
    private fun Map<String, J>.field(name: String): J = this[name]
        ?: fail("missing required field " + name)
    // Explicit JSON null remains J.Null, never mistaken for an omitted field.
    private fun Map<String, J>.optional(name: String): J? = this[name]
    private fun Map<String, J>.text(name: String): String = field(name).str(name)
    private fun fail(message: String): Nothing = throw UiProtocolException(message)
}

/** A single commit point: invalid bytes or patches cannot modify UI state. */
fun UiRuntimeState.applyJson(json: String) {
    apply(NulangUiJsonCodec.decodeRuntimeMessage(json))
}
