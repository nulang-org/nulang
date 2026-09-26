import Foundation
import XCTest
@testable import NulangUIProtocol

final class ProtocolTests: XCTestCase {
    private func leaf(_ id: String, kind: String = "text") -> UiNode {
        UiNode(id: id, kind: kind)
    }

    private func document(revision: UInt64 = 1) -> UiDocument {
        UiDocument(
            documentId: "tasks",
            revision: Revision(revision),
            root: "root",
            nodes: [
                UiNode(id: "root", kind: "column", children: ["task-1"]),
                UiNode(
                    id: "task-1",
                    kind: "text",
                    properties: ["text": .string("first task")]
                ),
            ]
        )
    }

    func testRevisionRoundTripsFullUInt64RangeAsString() throws {
        let encoder = JSONEncoder()
        let data = try encoder.encode(Revision(UInt64.max))
        XCTAssertEqual(String(decoding: data, as: UTF8.self), "\"18446744073709551615\"")
        XCTAssertEqual(try JSONDecoder().decode(Revision.self, from: data), Revision(UInt64.max))
    }

    func testWireValuesPreserveLosslessIntegerAndFloatBits() throws {
        let values: [WireValue] = [
            .null,
            .bool(true),
            .i64(LosslessI64(Int64.min)),
            .i64(LosslessI64(Int64.max)),
            .f64(F64Bits(bits: 0x7ff8_0000_0000_0042)),
            .string("héllo 🌍"),
            .bytes([0, 1, 127, 128, 255]),
            .array([.string("nested")]),
            .object(["z": .i64(LosslessI64(2)), "a": .i64(LosslessI64(1))]),
        ]

        for value in values {
            let data = try JSONEncoder().encode(value)
            XCTAssertEqual(try JSONDecoder().decode(WireValue.self, from: data), value)
        }

        let integerData = try JSONEncoder().encode(WireValue.i64(LosslessI64(Int64.max)))
        let integerJson = String(decoding: integerData, as: UTF8.self)
        XCTAssertTrue(integerJson.contains("\"9223372036854775807\""))

        let bits = F64Bits(bits: 0x7ff8_0000_0000_0042)
        XCTAssertEqual(bits.value.bitPattern, 0x7ff8_0000_0000_0042)
        let floatData = try JSONEncoder().encode(WireValue.f64(bits))
        XCTAssertTrue(String(decoding: floatData, as: UTF8.self).contains("7ff8000000000042"))
    }

    func testDocumentRejectsCyclesOrphansAndWrongVersions() throws {
        let cycle = UiDocument(
            documentId: "cycle",
            revision: Revision(1),
            root: "root",
            nodes: [
                UiNode(id: "root", kind: "column", children: ["child"]),
                UiNode(id: "child", kind: "column", children: ["root"]),
            ]
        )
        XCTAssertThrowsError(try cycle.validate()) { error in
            guard case NulangUIProtocolError.cycle = error else {
                return XCTFail("expected cycle, got \(error)")
            }
        }

        let orphan = UiDocument(
            documentId: "orphan",
            revision: Revision(1),
            root: "root",
            nodes: [leaf("root", kind: "column"), leaf("detached")]
        )
        XCTAssertThrowsError(try orphan.validate()) { error in
            guard case NulangUIProtocolError.unreachableNode = error else {
                return XCTFail("expected unreachable node, got \(error)")
            }
        }

        var wrong = document()
        wrong.protocolVersion = "nulang-ui/999"
        XCTAssertThrowsError(try wrong.validate()) { error in
            guard case NulangUIProtocolError.unsupportedProtocol = error else {
                return XCTFail("expected unsupported protocol, got \(error)")
            }
        }
    }

    func testPatchApplicationIsOrderedRevisionCheckedAndAtomic() throws {
        var state = document()
        let patch = UiPatch(
            documentId: "tasks",
            baseRevision: Revision(1),
            revision: Revision(2),
            operations: [
                .upsertNode(leaf("task-2")),
                .setProperty(nodeId: "task-2", name: "text", value: .string("second task")),
                .replaceChildren(nodeId: "root", children: ["task-1", "task-2"]),
            ]
        )
        try state.apply(patch)
        XCTAssertEqual(state.revision, Revision(2))
        XCTAssertEqual(state.node(id: "task-2")?.properties["text"], .string("second task"))

        XCTAssertThrowsError(try state.apply(patch)) { error in
            guard case NulangUIProtocolError.revisionMismatch = error else {
                return XCTFail("expected revision mismatch, got \(error)")
            }
        }

        let before = state
        let invalid = UiPatch(
            documentId: "tasks",
            baseRevision: Revision(2),
            revision: Revision(3),
            operations: [
                .setProperty(nodeId: "task-1", name: "text", value: .string("changed")),
                .removeNode("missing"),
            ]
        )
        XCTAssertThrowsError(try state.apply(invalid))
        XCTAssertEqual(state, before, "failed patches must be atomic")
    }

    func testRuntimeMessageStateRequiresSnapshotBeforePatch() throws {
        var state = UiRuntimeState()
        let patch = UiPatch(
            documentId: "tasks",
            baseRevision: Revision(1),
            revision: Revision(2),
            operations: []
        )
        XCTAssertThrowsError(
            try state.apply(.patch(protocolVersion: nulangUIMessageProtocolVersion, patch: patch))
        ) { error in
            XCTAssertEqual(error as? NulangUIProtocolError, .missingSnapshot)
        }

        try state.apply(
            .snapshot(protocolVersion: nulangUIMessageProtocolVersion, document: document())
        )
        try state.apply(
            .patch(protocolVersion: nulangUIMessageProtocolVersion, patch: patch)
        )
        XCTAssertEqual(state.document?.revision, Revision(2))
    }

    func testActionEnvelopeMatchesFrozenABI() throws {
        let request = ActionRequest(
            documentId: "tasks",
            revision: Revision(7),
            actionId: "task.complete",
            placement: .server,
            correlationId: "trace-123",
            idempotencyKey: "mutation-456",
            payload: .i64(LosslessI64(Int64.max))
        )
        let message = HostToRuntimeMessage.invokeAction(
            protocolVersion: nulangUIMessageProtocolVersion,
            request: request
        )

        let json = try NulangUICodec.encodeHostMessageJSON(message)
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]
        )
        XCTAssertEqual(object["type"] as? String, "invoke_action")
        XCTAssertEqual(object["protocol"] as? String, nulangUIMessageProtocolVersion)

        let requestObject = try XCTUnwrap(object["request"] as? [String: Any])
        XCTAssertEqual(requestObject["document_id"] as? String, "tasks")
        XCTAssertEqual(requestObject["revision"] as? String, "7")
        XCTAssertEqual(requestObject["action_id"] as? String, "task.complete")
        XCTAssertEqual(requestObject["placement"] as? String, "server")
        XCTAssertEqual(requestObject["correlation_id"] as? String, "trace-123")
        XCTAssertEqual(requestObject["idempotency_key"] as? String, "mutation-456")

        let payloadObject = try XCTUnwrap(requestObject["payload"] as? [String: Any])
        XCTAssertEqual(payloadObject["type"] as? String, "i64")
        XCTAssertEqual(payloadObject["value"] as? String, "9223372036854775807")

        let decoded = try JSONDecoder().decode(
            HostToRuntimeMessage.self,
            from: Data(json.utf8)
        )
        XCTAssertEqual(decoded, message)
    }

    func testRuntimeJSONMatchesRustTaggedShape() throws {
        let message = RuntimeToHostMessage.snapshot(
            protocolVersion: nulangUIMessageProtocolVersion,
            document: document()
        )
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let data = try encoder.encode(message)
        let json = String(decoding: data, as: UTF8.self)
        XCTAssertTrue(json.contains("\"type\":\"snapshot\""))
        XCTAssertTrue(json.contains("\"document_id\":\"tasks\""))
        XCTAssertTrue(json.contains("\"revision\":\"1\""))

        let decoded = try NulangUICodec.decodeRuntimeMessage(json)
        XCTAssertEqual(decoded, message)
        try decoded.validate()
    }

    func testDeepTreeValidationIsIterative() throws {
        let depth = 20_000
        var nodes: [UiNode] = []
        nodes.reserveCapacity(depth)
        for index in 0..<depth {
            let id = String(format: "node-%05d", index)
            let children = index + 1 < depth
                ? [String(format: "node-%05d", index + 1)]
                : []
            nodes.append(UiNode(id: id, kind: "column", children: children))
        }
        let deep = UiDocument(
            documentId: "deep",
            revision: Revision(1),
            root: "node-00000",
            nodes: nodes
        )
        XCTAssertNoThrow(try deep.validate())
    }
}
