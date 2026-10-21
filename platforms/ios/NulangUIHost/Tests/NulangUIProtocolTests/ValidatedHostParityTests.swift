import Foundation
import XCTest
@testable import NulangUIProtocol

/// Contract parity with nulang-ui-protocol's validated JSON decoder and
/// per-node event uniqueness. The Swift host must fail closed before rendering.
final class ValidatedHostParityTests: XCTestCase {
    private func document(
        protocolVersion: String = nulangUIProtocolVersion,
        nodes: [UiNode]? = nil
    ) -> UiDocument {
        UiDocument(
            documentId: "tasks",
            revision: Revision(1),
            root: "button",
            nodes: nodes ?? [UiNode(id: "button", kind: "button")]
        ).withProtocolVersion(protocolVersion)
    }

    private func decode(_ message: RuntimeToHostMessage) throws -> RuntimeToHostMessage {
        let data = try JSONEncoder().encode(message)
        return try NulangUICodec.decodeRuntimeMessage(data)
    }

    func testDecoderRejectsUnsupportedMessageProtocol() throws {
        let message = RuntimeToHostMessage.snapshot(
            protocolVersion: "nulang-ui-msg/999",
            document: document()
        )
        XCTAssertThrowsError(try decode(message))
    }

    func testDecoderRejectsUnreachableSnapshotNode() throws {
        let message = RuntimeToHostMessage.snapshot(
            protocolVersion: nulangUIMessageProtocolVersion,
            document: document(nodes: [
                UiNode(id: "button", kind: "button"),
                UiNode(id: "orphan", kind: "text"),
            ])
        )
        XCTAssertThrowsError(try decode(message))
    }

    func testDecoderRejectsNonIncreasingPatchRevision() throws {
        let message = RuntimeToHostMessage.patch(
            protocolVersion: nulangUIMessageProtocolVersion,
            patch: UiPatch(
                documentId: "tasks",
                baseRevision: Revision(5),
                revision: Revision(5),
                operations: []
            )
        )
        XCTAssertThrowsError(try decode(message))
    }

    func testDecoderRejectsDuplicateActionEventOnOneNode() throws {
        let node = UiNode(
            id: "button",
            kind: "button",
            actions: [
                ActionBinding(event: "press", actionId: "one", placement: .client),
                ActionBinding(event: "press", actionId: "two", placement: .client),
            ]
        )
        let message = RuntimeToHostMessage.snapshot(
            protocolVersion: nulangUIMessageProtocolVersion,
            document: document(nodes: [node])
        )
        XCTAssertThrowsError(try decode(message))
    }

    func testDistinctActionEventsRemainValid() throws {
        let node = UiNode(
            id: "button",
            kind: "button",
            actions: [
                ActionBinding(event: "press", actionId: "one", placement: .client),
                ActionBinding(event: "tap", actionId: "two", placement: .client),
            ]
        )
        let message = RuntimeToHostMessage.snapshot(
            protocolVersion: nulangUIMessageProtocolVersion,
            document: document(nodes: [node])
        )
        XCTAssertEqual(try decode(message), message)
    }

    func testInvalidReplaceActionsDoesNotCommitNewRevision() throws {
        var state = UiRuntimeState()
        try state.apply(.snapshot(
            protocolVersion: nulangUIMessageProtocolVersion,
            document: document()
        ))
        let original = state
        let patch = UiPatch(
            documentId: "tasks",
            baseRevision: Revision(1),
            revision: Revision(2),
            operations: [
                .replaceActions(nodeId: "button", actions: [
                    ActionBinding(event: "press", actionId: "one", placement: .client),
                    ActionBinding(event: "press", actionId: "two", placement: .client),
                ]),
            ]
        )
        XCTAssertThrowsError(try state.apply(.patch(
            protocolVersion: nulangUIMessageProtocolVersion,
            patch: patch
        )))
        XCTAssertEqual(state, original)
    }
}

private extension UiDocument {
    func withProtocolVersion(_ value: String) -> UiDocument {
        var document = self
        document.protocolVersion = value
        return document
    }
}
