package org.nulang.ui

import java.util.ArrayDeque

/**
 * Kotlin/JVM reference state machine for the renderer-neutral nulang-ui/1
 * contract. This layer does not parse untrusted JSON, authorize actions,
 * schedule work, or own any Android UI controls.
 */
object UiProtocol {
    const val DOCUMENT_VERSION = "nulang-ui/1"
    const val MESSAGE_VERSION = "nulang-ui-msg/1"
}

class UiProtocolException(message: String) : IllegalArgumentException(message)

@JvmInline
value class Revision(val value: ULong) {
    companion object {
        fun parse(wire: String): Revision {
            val digits = if (wire.startsWith('+')) wire.substring(1) else wire
            if (digits.isEmpty() || !digits.all { it in '0'..'9' }) {
                throw UiProtocolException("revision must contain unsigned decimal digits")
            }
            val parsed = digits.toULongOrNull()
                ?: throw UiProtocolException("revision outside UInt64 range")
            return Revision(parsed)
        }
    }
    fun wire(): String = value.toString()
}

enum class ActionPlacement { CLIENT, SERVER }

data class ActionBinding(
    val event: String,
    val actionId: String,
    val placement: ActionPlacement
)

/** Host types stay lossless. A future JSON codec must not coerce tagged integers via Double. */
sealed interface WireValue {
    data object Null : WireValue
    data class Bool(val value: Boolean) : WireValue
    data class Text(val value: String) : WireValue
    data class SignedInt(val value: Long) : WireValue
    data class FloatBits(val bits: ULong) : WireValue
    data class Bytes(val value: List<UByte>) : WireValue
    data class Array(val value: List<WireValue>) : WireValue
    data class Object(val value: Map<String, WireValue>) : WireValue
}

data class UiNode(
    val id: String,
    val kind: String,
    val children: List<String> = emptyList(),
    val actions: List<ActionBinding> = emptyList(),
    val properties: Map<String, WireValue> = emptyMap()
)

data class UiDocument(
    val documentId: String,
    val revision: Revision,
    val root: String,
    val nodes: List<UiNode>,
    val protocol: String = UiProtocol.DOCUMENT_VERSION
) {
    fun validate() {
        requireVersion(UiProtocol.DOCUMENT_VERSION, protocol)
        requireId("document_id", documentId)
        requireId("root", root)
        val index = LinkedHashMap<String, UiNode>(nodes.size)
        for (node in nodes) {
            requireId("node_id", node.id)
            if (node.kind.isBlank()) throw UiProtocolException("node ${node.id} has empty kind")
            val events = HashSet<String>()
            for (binding in node.actions) {
                requireId("action_id", binding.actionId)
                if (binding.event.isBlank()) throw UiProtocolException("node ${node.id} has empty action event")
                if (!events.add(binding.event)) {
                    throw UiProtocolException("node ${node.id} has duplicate action event ${binding.event}")
                }
            }
            if (index.putIfAbsent(node.id, node) != null) {
                throw UiProtocolException("duplicate node id ${node.id}")
            }
        }
        if (!index.containsKey(root)) throw UiProtocolException("missing root node $root")
        val parentCounts = HashMap<String, Int>()
        for (node in nodes) {
            val childSet = HashSet<String>()
            for (child in node.children) {
                if (!index.containsKey(child)) throw UiProtocolException("missing child $child of ${node.id}")
                if (!childSet.add(child)) throw UiProtocolException("duplicate child $child of ${node.id}")
                val count = (parentCounts[child] ?: 0) + 1
                if (count > 1) throw UiProtocolException("node $child has multiple parents")
                parentCounts[child] = count
            }
        }
        // Iterative graph traversal is required: the shared Rust contract
        // accepts documents at least 20,000 levels deep.
        val visiting = HashSet<String>()
        val visited = HashSet<String>()
        val stack = ArrayDeque<Pair<String, Boolean>>()
        stack.addLast(root to false)
        while (stack.isNotEmpty()) {
            val (id, exiting) = stack.removeLast()
            if (exiting) {
                visiting.remove(id)
                visited.add(id)
                continue
            }
            if (id in visited) continue
            if (!visiting.add(id)) throw UiProtocolException("cycle at $id")
            stack.addLast(id to true)
            for (child in index.getValue(id).children.asReversed()) {
                if (child in visiting) throw UiProtocolException("cycle at $child")
                if (child !in visited) stack.addLast(child to false)
            }
        }
        if (visited.size != nodes.size) throw UiProtocolException("unreachable node")
    }

    /** All intermediate mutations are local; invalid final state is never published. */
    fun applyPatch(patch: UiPatch): UiDocument {
        validate()
        patch.validate()
        if (patch.documentId != documentId) throw UiProtocolException("patch document mismatch")
        if (patch.baseRevision != revision) throw UiProtocolException("patch base revision mismatch")
        val next = nodes.associateBy { it.id }.toMutableMap()
        var nextRoot = root
        fun requireNode(id: String): UiNode = next[id]
            ?: throw UiProtocolException("node $id does not exist")
        for (op in patch.operations) when (op) {
            is PatchOperation.UpsertNode -> next[op.node.id] = op.node
            is PatchOperation.RemoveNode -> {
                if (next.remove(op.nodeId) == null) throw UiProtocolException("node ${op.nodeId} does not exist")
            }
            is PatchOperation.SetRoot -> nextRoot = op.nodeId
            is PatchOperation.ReplaceChildren -> next[op.nodeId] = requireNode(op.nodeId).copy(children = op.children)
            is PatchOperation.ReplaceActions -> next[op.nodeId] = requireNode(op.nodeId).copy(actions = op.actions)
            is PatchOperation.SetProperty -> next[op.nodeId] = requireNode(op.nodeId).let {
                it.copy(properties = it.properties + (op.name to op.value))
            }
            is PatchOperation.RemoveProperty -> next[op.nodeId] = requireNode(op.nodeId).let {
                it.copy(properties = it.properties - op.name)
            }
        }
        return copy(revision = patch.revision, root = nextRoot, nodes = next.values.sortedBy { it.id }).also { it.validate() }
    }
}

sealed interface PatchOperation {
    data class UpsertNode(val node: UiNode) : PatchOperation
    data class RemoveNode(val nodeId: String) : PatchOperation
    data class SetRoot(val nodeId: String) : PatchOperation
    data class ReplaceChildren(val nodeId: String, val children: List<String>) : PatchOperation
    data class ReplaceActions(val nodeId: String, val actions: List<ActionBinding>) : PatchOperation
    data class SetProperty(val nodeId: String, val name: String, val value: WireValue) : PatchOperation
    data class RemoveProperty(val nodeId: String, val name: String) : PatchOperation
}

data class UiPatch(
    val documentId: String,
    val baseRevision: Revision,
    val revision: Revision,
    val operations: List<PatchOperation>,
    val protocol: String = UiProtocol.DOCUMENT_VERSION
) {
    fun validate() {
        requireVersion(UiProtocol.DOCUMENT_VERSION, protocol)
        requireId("document_id", documentId)
        if (revision.value <= baseRevision.value) {
            throw UiProtocolException("patch revision must increase")
        }
    }
}

sealed interface RuntimeMessage {
    val protocol: String
    data class Snapshot(val document: UiDocument, override val protocol: String = UiProtocol.MESSAGE_VERSION) : RuntimeMessage
    data class Patch(val patch: UiPatch, override val protocol: String = UiProtocol.MESSAGE_VERSION) : RuntimeMessage
}

/** Not thread-safe. Android's eventual UI adapter must serialize publication on the UI thread. */
class UiRuntimeState(initial: UiDocument? = null) {
    var document: UiDocument? = initial?.also { it.validate() }
        private set

    fun apply(message: RuntimeMessage) {
        requireVersion(UiProtocol.MESSAGE_VERSION, message.protocol)
        val next = when (message) {
            is RuntimeMessage.Snapshot -> message.document.also { it.validate() }
            is RuntimeMessage.Patch -> {
                val current = document ?: throw UiProtocolException("missing initial snapshot")
                current.applyPatch(message.patch)
            }
        }
        document = next
    }
}

private fun requireVersion(expected: String, actual: String) {
    if (actual != expected) throw UiProtocolException("unsupported protocol $actual; expected $expected")
}
private fun requireId(field: String, value: String) {
    if (value.isBlank()) throw UiProtocolException("$field must not be empty")
}
