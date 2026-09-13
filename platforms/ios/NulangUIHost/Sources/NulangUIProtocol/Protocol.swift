import Foundation

public let nulangUIProtocolVersion = "nulang-ui/1"
public let nulangUIMessageProtocolVersion = "nulang-ui-msg/1"

public struct Revision: Codable, Hashable, Comparable, Sendable {
    public let rawValue: UInt64

    public init(_ rawValue: UInt64) {
        self.rawValue = rawValue
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        let text = try container.decode(String.self)
        guard let value = UInt64(text) else {
            throw DecodingError.dataCorruptedError(
                in: container,
                debugDescription: "revision must be a decimal UInt64 string"
            )
        }
        self.rawValue = value
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(String(rawValue))
    }

    public static func < (lhs: Revision, rhs: Revision) -> Bool {
        lhs.rawValue < rhs.rawValue
    }
}

public struct LosslessI64: Codable, Hashable, Sendable {
    public let rawValue: Int64

    public init(_ rawValue: Int64) {
        self.rawValue = rawValue
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        let text = try container.decode(String.self)
        guard let value = Int64(text) else {
            throw DecodingError.dataCorruptedError(
                in: container,
                debugDescription: "i64 must be a decimal Int64 string"
            )
        }
        self.rawValue = value
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(String(rawValue))
    }
}

public struct F64Bits: Codable, Hashable, Sendable {
    public let rawValue: UInt64

    public init(bits: UInt64) {
        self.rawValue = bits
    }

    public init(_ value: Double) {
        self.rawValue = value.bitPattern
    }

    public var value: Double {
        Double(bitPattern: rawValue)
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        let text = try container.decode(String.self)
        guard text.count == 16, let value = UInt64(text, radix: 16) else {
            throw DecodingError.dataCorruptedError(
                in: container,
                debugDescription: "f64 bit payload must contain exactly 16 hexadecimal digits"
            )
        }
        self.rawValue = value
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(String(format: "%016llx", rawValue))
    }
}

public enum WireValue: Equatable, Sendable, Codable {
    case null
    case bool(Bool)
    case i64(LosslessI64)
    case f64(F64Bits)
    case string(String)
    case bytes([UInt8])
    case array([WireValue])
    case object([String: WireValue])

    private enum CodingKeys: String, CodingKey {
        case type
        case value
    }

    private enum Kind: String, Codable {
        case null
        case bool
        case i64
        case f64
        case string
        case bytes
        case array
        case object
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(Kind.self, forKey: .type)
        switch kind {
        case .null:
            self = .null
        case .bool:
            self = .bool(try container.decode(Bool.self, forKey: .value))
        case .i64:
            self = .i64(try container.decode(LosslessI64.self, forKey: .value))
        case .f64:
            self = .f64(try container.decode(F64Bits.self, forKey: .value))
        case .string:
            self = .string(try container.decode(String.self, forKey: .value))
        case .bytes:
            self = .bytes(try container.decode([UInt8].self, forKey: .value))
        case .array:
            self = .array(try container.decode([WireValue].self, forKey: .value))
        case .object:
            self = .object(try container.decode([String: WireValue].self, forKey: .value))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .null:
            try container.encode(Kind.null, forKey: .type)
        case .bool(let value):
            try container.encode(Kind.bool, forKey: .type)
            try container.encode(value, forKey: .value)
        case .i64(let value):
            try container.encode(Kind.i64, forKey: .type)
            try container.encode(value, forKey: .value)
        case .f64(let value):
            try container.encode(Kind.f64, forKey: .type)
            try container.encode(value, forKey: .value)
        case .string(let value):
            try container.encode(Kind.string, forKey: .type)
            try container.encode(value, forKey: .value)
        case .bytes(let value):
            try container.encode(Kind.bytes, forKey: .type)
            try container.encode(value, forKey: .value)
        case .array(let value):
            try container.encode(Kind.array, forKey: .type)
            try container.encode(value, forKey: .value)
        case .object(let value):
            try container.encode(Kind.object, forKey: .type)
            try container.encode(value, forKey: .value)
        }
    }

    public var stringValue: String? {
        if case .string(let value) = self { return value }
        return nil
    }

    public var boolValue: Bool? {
        if case .bool(let value) = self { return value }
        return nil
    }
}

public enum ActionPlacement: String, Codable, Equatable, Sendable {
    case client
    case server
}

public struct ActionBinding: Codable, Equatable, Sendable {
    public var event: String
    public var actionId: String
    public var placement: ActionPlacement

    enum CodingKeys: String, CodingKey {
        case event
        case actionId = "action_id"
        case placement
    }

    public init(event: String, actionId: String, placement: ActionPlacement) {
        self.event = event
        self.actionId = actionId
        self.placement = placement
    }
}

public struct UiNode: Codable, Equatable, Sendable, Identifiable {
    public var id: String
    public var kind: String
    public var properties: [String: WireValue]
    public var children: [String]
    public var actions: [ActionBinding]

    enum CodingKeys: String, CodingKey {
        case id
        case kind
        case properties
        case children
        case actions
    }

    public init(
        id: String,
        kind: String,
        properties: [String: WireValue] = [:],
        children: [String] = [],
        actions: [ActionBinding] = []
    ) {
        self.id = id
        self.kind = kind
        self.properties = properties
        self.children = children
        self.actions = actions
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        kind = try container.decode(String.self, forKey: .kind)
        properties = try container.decodeIfPresent([String: WireValue].self, forKey: .properties) ?? [:]
        children = try container.decodeIfPresent([String].self, forKey: .children) ?? []
        actions = try container.decodeIfPresent([ActionBinding].self, forKey: .actions) ?? []
    }
}

public enum NulangUIProtocolError: Error, Equatable, Sendable {
    case unsupportedProtocol(expected: String, found: String)
    case emptyIdentifier(String)
    case emptyNodeKind(String)
    case emptyActionEvent(String)
    case duplicateNode(String)
    case missingNode(String)
    case danglingChild(parent: String, child: String)
    case duplicateChild(parent: String, child: String)
    case multipleParents(String)
    case cycle(String)
    case unreachableNode(String)
    case documentIdMismatch(expected: String, found: String)
    case revisionMismatch(expected: Revision, found: Revision)
    case nonIncreasingRevision(base: Revision, next: Revision)
    case missingSnapshot
}

extension NulangUIProtocolError: LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .unsupportedProtocol(let expected, let found):
            return "unsupported protocol \(found); expected \(expected)"
        case .emptyIdentifier(let field):
            return "\(field) must not be empty"
        case .emptyNodeKind(let node):
            return "node \(node) has an empty kind"
        case .emptyActionEvent(let node):
            return "node \(node) has an empty action event"
        case .duplicateNode(let node):
            return "duplicate node id \(node)"
        case .missingNode(let node):
            return "node \(node) does not exist"
        case .danglingChild(let parent, let child):
            return "node \(parent) references missing child \(child)"
        case .duplicateChild(let parent, let child):
            return "node \(parent) references child \(child) more than once"
        case .multipleParents(let node):
            return "node \(node) has more than one parent"
        case .cycle(let node):
            return "semantic UI tree contains a cycle at \(node)"
        case .unreachableNode(let node):
            return "node \(node) is unreachable from the root"
        case .documentIdMismatch(let expected, let found):
            return "patch targets document \(found), expected \(expected)"
        case .revisionMismatch(let expected, let found):
            return "patch base revision is \(found.rawValue), expected \(expected.rawValue)"
        case .nonIncreasingRevision(let base, let next):
            return "patch revision \(next.rawValue) must be greater than base revision \(base.rawValue)"
        case .missingSnapshot:
            return "received a UI patch before an initial snapshot"
        }
    }
}

private func requireProtocol(_ expected: String, _ found: String) throws {
    guard expected == found else {
        throw NulangUIProtocolError.unsupportedProtocol(expected: expected, found: found)
    }
}

private func requireIdentifier(_ field: String, _ value: String) throws {
    guard !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
        throw NulangUIProtocolError.emptyIdentifier(field)
    }
}

public struct UiDocument: Codable, Equatable, Sendable {
    public var protocolVersion: String
    public var documentId: String
    public var revision: Revision
    public var root: String
    public var nodes: [UiNode]

    enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol"
        case documentId = "document_id"
        case revision
        case root
        case nodes
    }

    public init(
        protocolVersion: String = nulangUIProtocolVersion,
        documentId: String,
        revision: Revision,
        root: String,
        nodes: [UiNode]
    ) {
        self.protocolVersion = protocolVersion
        self.documentId = documentId
        self.revision = revision
        self.root = root
        self.nodes = nodes.sorted { $0.id < $1.id }
    }

    public func validate() throws {
        try requireProtocol(nulangUIProtocolVersion, protocolVersion)
        try requireIdentifier("document_id", documentId)
        try requireIdentifier("root", root)

        var byId: [String: UiNode] = [:]
        for node in nodes {
            try requireIdentifier("node_id", node.id)
            guard !node.kind.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                throw NulangUIProtocolError.emptyNodeKind(node.id)
            }
            for binding in node.actions {
                try requireIdentifier("action_id", binding.actionId)
                guard !binding.event.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                    throw NulangUIProtocolError.emptyActionEvent(node.id)
                }
            }
            guard byId[node.id] == nil else {
                throw NulangUIProtocolError.duplicateNode(node.id)
            }
            byId[node.id] = node
        }

        guard byId[root] != nil else {
            throw NulangUIProtocolError.missingNode(root)
        }

        var parentCounts: [String: Int] = [:]
        for node in nodes {
            var local = Set<String>()
            for child in node.children {
                guard byId[child] != nil else {
                    throw NulangUIProtocolError.danglingChild(parent: node.id, child: child)
                }
                guard local.insert(child).inserted else {
                    throw NulangUIProtocolError.duplicateChild(parent: node.id, child: child)
                }
                parentCounts[child, default: 0] += 1
                if parentCounts[child, default: 0] > 1 {
                    throw NulangUIProtocolError.multipleParents(child)
                }
            }
        }

        var visiting = Set<String>()
        var visited = Set<String>()
        var stack: [(String, Bool)] = [(root, false)]
        while let (nodeId, exiting) = stack.popLast() {
            if exiting {
                visiting.remove(nodeId)
                visited.insert(nodeId)
                continue
            }
            if visited.contains(nodeId) {
                continue
            }
            guard visiting.insert(nodeId).inserted else {
                throw NulangUIProtocolError.cycle(nodeId)
            }

            stack.append((nodeId, true))
            guard let node = byId[nodeId] else {
                throw NulangUIProtocolError.missingNode(nodeId)
            }
            for child in node.children.reversed() {
                if visiting.contains(child) {
                    throw NulangUIProtocolError.cycle(child)
                }
                if !visited.contains(child) {
                    stack.append((child, false))
                }
            }
        }

        if visited.count != byId.count,
           let unreachable = byId.keys.sorted().first(where: { !visited.contains($0) }) {
            throw NulangUIProtocolError.unreachableNode(unreachable)
        }
    }

    public func node(id: String) -> UiNode? {
        nodes.first { $0.id == id }
    }

    public mutating func apply(_ patch: UiPatch) throws {
        try validate()
        try patch.validate()
        guard documentId == patch.documentId else {
            throw NulangUIProtocolError.documentIdMismatch(
                expected: documentId,
                found: patch.documentId
            )
        }
        guard revision == patch.baseRevision else {
            throw NulangUIProtocolError.revisionMismatch(
                expected: revision,
                found: patch.baseRevision
            )
        }

        var next = self
        for operation in patch.operations {
            try next.apply(operation)
        }
        next.revision = patch.revision
        next.nodes.sort { $0.id < $1.id }
        try next.validate()
        self = next
    }

    private mutating func apply(_ operation: PatchOperation) throws {
        switch operation {
        case .upsertNode(let node):
            if let index = nodes.firstIndex(where: { $0.id == node.id }) {
                nodes[index] = node
            } else {
                nodes.append(node)
            }
        case .removeNode(let nodeId):
            guard let index = nodes.firstIndex(where: { $0.id == nodeId }) else {
                throw NulangUIProtocolError.missingNode(nodeId)
            }
            nodes.remove(at: index)
        case .setRoot(let nodeId):
            root = nodeId
        case .setProperty(let nodeId, let name, let value):
            guard let index = nodes.firstIndex(where: { $0.id == nodeId }) else {
                throw NulangUIProtocolError.missingNode(nodeId)
            }
            nodes[index].properties[name] = value
        case .removeProperty(let nodeId, let name):
            guard let index = nodes.firstIndex(where: { $0.id == nodeId }) else {
                throw NulangUIProtocolError.missingNode(nodeId)
            }
            nodes[index].properties.removeValue(forKey: name)
        case .replaceChildren(let nodeId, let children):
            guard let index = nodes.firstIndex(where: { $0.id == nodeId }) else {
                throw NulangUIProtocolError.missingNode(nodeId)
            }
            nodes[index].children = children
        case .replaceActions(let nodeId, let actions):
            guard let index = nodes.firstIndex(where: { $0.id == nodeId }) else {
                throw NulangUIProtocolError.missingNode(nodeId)
            }
            nodes[index].actions = actions
        }
    }
}

public enum PatchOperation: Equatable, Sendable, Codable {
    case upsertNode(UiNode)
    case removeNode(String)
    case setRoot(String)
    case setProperty(nodeId: String, name: String, value: WireValue)
    case removeProperty(nodeId: String, name: String)
    case replaceChildren(nodeId: String, children: [String])
    case replaceActions(nodeId: String, actions: [ActionBinding])

    private enum CodingKeys: String, CodingKey {
        case op
        case node
        case nodeId = "node_id"
        case name
        case value
        case children
        case actions
    }

    private enum Operation: String, Codable {
        case upsertNode = "upsert_node"
        case removeNode = "remove_node"
        case setRoot = "set_root"
        case setProperty = "set_property"
        case removeProperty = "remove_property"
        case replaceChildren = "replace_children"
        case replaceActions = "replace_actions"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(Operation.self, forKey: .op) {
        case .upsertNode:
            self = .upsertNode(try container.decode(UiNode.self, forKey: .node))
        case .removeNode:
            self = .removeNode(try container.decode(String.self, forKey: .nodeId))
        case .setRoot:
            self = .setRoot(try container.decode(String.self, forKey: .nodeId))
        case .setProperty:
            self = .setProperty(
                nodeId: try container.decode(String.self, forKey: .nodeId),
                name: try container.decode(String.self, forKey: .name),
                value: try container.decode(WireValue.self, forKey: .value)
            )
        case .removeProperty:
            self = .removeProperty(
                nodeId: try container.decode(String.self, forKey: .nodeId),
                name: try container.decode(String.self, forKey: .name)
            )
        case .replaceChildren:
            self = .replaceChildren(
                nodeId: try container.decode(String.self, forKey: .nodeId),
                children: try container.decode([String].self, forKey: .children)
            )
        case .replaceActions:
            self = .replaceActions(
                nodeId: try container.decode(String.self, forKey: .nodeId),
                actions: try container.decode([ActionBinding].self, forKey: .actions)
            )
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .upsertNode(let node):
            try container.encode(Operation.upsertNode, forKey: .op)
            try container.encode(node, forKey: .node)
        case .removeNode(let nodeId):
            try container.encode(Operation.removeNode, forKey: .op)
            try container.encode(nodeId, forKey: .nodeId)
        case .setRoot(let nodeId):
            try container.encode(Operation.setRoot, forKey: .op)
            try container.encode(nodeId, forKey: .nodeId)
        case .setProperty(let nodeId, let name, let value):
            try container.encode(Operation.setProperty, forKey: .op)
            try container.encode(nodeId, forKey: .nodeId)
            try container.encode(name, forKey: .name)
            try container.encode(value, forKey: .value)
        case .removeProperty(let nodeId, let name):
            try container.encode(Operation.removeProperty, forKey: .op)
            try container.encode(nodeId, forKey: .nodeId)
            try container.encode(name, forKey: .name)
        case .replaceChildren(let nodeId, let children):
            try container.encode(Operation.replaceChildren, forKey: .op)
            try container.encode(nodeId, forKey: .nodeId)
            try container.encode(children, forKey: .children)
        case .replaceActions(let nodeId, let actions):
            try container.encode(Operation.replaceActions, forKey: .op)
            try container.encode(nodeId, forKey: .nodeId)
            try container.encode(actions, forKey: .actions)
        }
    }
}

public struct UiPatch: Codable, Equatable, Sendable {
    public var protocolVersion: String
    public var documentId: String
    public var baseRevision: Revision
    public var revision: Revision
    public var operations: [PatchOperation]

    enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol"
        case documentId = "document_id"
        case baseRevision = "base_revision"
        case revision
        case operations
    }

    public init(
        protocolVersion: String = nulangUIProtocolVersion,
        documentId: String,
        baseRevision: Revision,
        revision: Revision,
        operations: [PatchOperation]
    ) {
        self.protocolVersion = protocolVersion
        self.documentId = documentId
        self.baseRevision = baseRevision
        self.revision = revision
        self.operations = operations
    }

    public func validate() throws {
        try requireProtocol(nulangUIProtocolVersion, protocolVersion)
        try requireIdentifier("document_id", documentId)
        guard revision > baseRevision else {
            throw NulangUIProtocolError.nonIncreasingRevision(
                base: baseRevision,
                next: revision
            )
        }
    }
}

public struct ActionRequest: Codable, Equatable, Sendable {
    public var documentId: String
    public var revision: Revision
    public var actionId: String
    public var placement: ActionPlacement
    public var correlationId: String
    public var idempotencyKey: String
    public var payload: WireValue

    enum CodingKeys: String, CodingKey {
        case documentId = "document_id"
        case revision
        case actionId = "action_id"
        case placement
        case correlationId = "correlation_id"
        case idempotencyKey = "idempotency_key"
        case payload
    }

    public init(
        documentId: String,
        revision: Revision,
        actionId: String,
        placement: ActionPlacement,
        correlationId: String,
        idempotencyKey: String,
        payload: WireValue
    ) {
        self.documentId = documentId
        self.revision = revision
        self.actionId = actionId
        self.placement = placement
        self.correlationId = correlationId
        self.idempotencyKey = idempotencyKey
        self.payload = payload
    }

    public func validate() throws {
        try requireIdentifier("document_id", documentId)
        try requireIdentifier("action_id", actionId)
        try requireIdentifier("correlation_id", correlationId)
        try requireIdentifier("idempotency_key", idempotencyKey)
    }
}

public enum RuntimeToHostMessage: Equatable, Sendable, Codable {
    case snapshot(protocolVersion: String, document: UiDocument)
    case patch(protocolVersion: String, patch: UiPatch)

    private enum CodingKeys: String, CodingKey {
        case type
        case protocolVersion = "protocol"
        case document
        case patch
    }

    private enum Kind: String, Codable {
        case snapshot
        case patch
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(Kind.self, forKey: .type)
        let protocolVersion = try container.decode(String.self, forKey: .protocolVersion)
        switch kind {
        case .snapshot:
            self = .snapshot(
                protocolVersion: protocolVersion,
                document: try container.decode(UiDocument.self, forKey: .document)
            )
        case .patch:
            self = .patch(
                protocolVersion: protocolVersion,
                patch: try container.decode(UiPatch.self, forKey: .patch)
            )
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .snapshot(let protocolVersion, let document):
            try container.encode(Kind.snapshot, forKey: .type)
            try container.encode(protocolVersion, forKey: .protocolVersion)
            try container.encode(document, forKey: .document)
        case .patch(let protocolVersion, let patch):
            try container.encode(Kind.patch, forKey: .type)
            try container.encode(protocolVersion, forKey: .protocolVersion)
            try container.encode(patch, forKey: .patch)
        }
    }

    public func validate() throws {
        switch self {
        case .snapshot(let protocolVersion, let document):
            try requireProtocol(nulangUIMessageProtocolVersion, protocolVersion)
            try document.validate()
        case .patch(let protocolVersion, let patch):
            try requireProtocol(nulangUIMessageProtocolVersion, protocolVersion)
            try patch.validate()
        }
    }
}

public enum HostToRuntimeMessage: Equatable, Sendable, Codable {
    case invokeAction(protocolVersion: String, request: ActionRequest)

    private enum CodingKeys: String, CodingKey {
        case type
        case protocolVersion = "protocol"
        case request
    }

    private enum Kind: String, Codable {
        case invokeAction = "invoke_action"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        _ = try container.decode(Kind.self, forKey: .type)
        self = .invokeAction(
            protocolVersion: try container.decode(String.self, forKey: .protocolVersion),
            request: try container.decode(ActionRequest.self, forKey: .request)
        )
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .invokeAction(let protocolVersion, let request):
            try container.encode(Kind.invokeAction, forKey: .type)
            try container.encode(protocolVersion, forKey: .protocolVersion)
            try container.encode(request, forKey: .request)
        }
    }

    public func validate() throws {
        switch self {
        case .invokeAction(let protocolVersion, let request):
            try requireProtocol(nulangUIMessageProtocolVersion, protocolVersion)
            try request.validate()
        }
    }
}

public enum NulangUICodec {
    public static func decodeRuntimeMessage(_ data: Data) throws -> RuntimeToHostMessage {
        try JSONDecoder().decode(RuntimeToHostMessage.self, from: data)
    }

    public static func decodeRuntimeMessage(_ json: String) throws -> RuntimeToHostMessage {
        guard let data = json.data(using: .utf8) else {
            throw CocoaError(.fileReadInapplicableStringEncoding)
        }
        return try decodeRuntimeMessage(data)
    }

    public static func encodeHostMessage(_ message: HostToRuntimeMessage) throws -> Data {
        try message.validate()
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        return try encoder.encode(message)
    }

    public static func encodeHostMessageJSON(_ message: HostToRuntimeMessage) throws -> String {
        let data = try encodeHostMessage(message)
        guard let json = String(data: data, encoding: .utf8) else {
            throw CocoaError(.fileWriteInapplicableStringEncoding)
        }
        return json
    }
}

public struct UiRuntimeState: Equatable, Sendable {
    public private(set) var document: UiDocument?

    public init(document: UiDocument? = nil) {
        self.document = document
    }

    public mutating func apply(_ message: RuntimeToHostMessage) throws {
        try message.validate()
        switch message {
        case .snapshot(_, let document):
            self.document = document
        case .patch(_, let patch):
            guard var document = self.document else {
                throw NulangUIProtocolError.missingSnapshot
            }
            try document.apply(patch)
            self.document = document
        }
    }

    public mutating func apply(json: String) throws {
        try apply(NulangUICodec.decodeRuntimeMessage(json))
    }
}
