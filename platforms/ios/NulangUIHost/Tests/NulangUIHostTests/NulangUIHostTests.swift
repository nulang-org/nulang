import XCTest
@testable import NulangUIHost
import NulangUIProtocol

@MainActor
final class NulangUIHostTests: XCTestCase {
    private func snapshot() -> RuntimeToHostMessage {
        .snapshot(
            protocolVersion: nulangUIMessageProtocolVersion,
            document: UiDocument(
                documentId: "tasks",
                revision: Revision(1),
                root: "root",
                nodes: [
                    UiNode(
                        id: "root",
                        kind: "column",
                        children: ["title", "complete"]
                    ),
                    UiNode(
                        id: "title",
                        kind: "text",
                        properties: ["text": .string("Ship native host")]
                    ),
                    UiNode(
                        id: "complete",
                        kind: "button",
                        properties: ["label": .string("Complete")],
                        actions: [
                            ActionBinding(
                                event: "press",
                                actionId: "task.complete",
                                placement: .server
                            )
                        ]
                    ),
                ]
            )
        )
    }

    func testStoreAppliesSnapshotAndPatch() throws {
        let store = NulangUIStore()
        store.applyRuntimeMessage(snapshot())
        XCTAssertEqual(store.document?.revision, Revision(1))
        XCTAssertEqual(
            store.document?.node(id: "title")?.properties["text"],
            .string("Ship native host")
        )

        let patch = UiPatch(
            documentId: "tasks",
            baseRevision: Revision(1),
            revision: Revision(2),
            operations: [
                .setProperty(
                    nodeId: "title",
                    name: "text",
                    value: .string("Native host shipped")
                )
            ]
        )
        store.applyRuntimeMessage(
            .patch(protocolVersion: nulangUIMessageProtocolVersion, patch: patch)
        )

        XCTAssertNil(store.lastError)
        XCTAssertEqual(store.document?.revision, Revision(2))
        XCTAssertEqual(
            store.document?.node(id: "title")?.properties["text"],
            .string("Native host shipped")
        )
    }

    func testInvalidPatchDoesNotPublishPartialState() throws {
        let store = NulangUIStore()
        store.applyRuntimeMessage(snapshot())
        let before = store.document

        let invalid = UiPatch(
            documentId: "tasks",
            baseRevision: Revision(1),
            revision: Revision(2),
            operations: [
                .setProperty(nodeId: "title", name: "text", value: .string("partial")),
                .removeNode("missing"),
            ]
        )
        store.applyRuntimeMessage(
            .patch(protocolVersion: nulangUIMessageProtocolVersion, patch: invalid)
        )

        XCTAssertNotNil(store.lastError)
        XCTAssertEqual(store.document, before)
    }

    func testButtonActionPreservesPlacementAndDeliveryMetadata() throws {
        var emitted: HostToRuntimeMessage?
        let store = NulangUIStore { message in
            emitted = message
        }
        store.applyRuntimeMessage(snapshot())
        store.sendAction(nodeId: "complete", event: "press")

        guard case .some(.invokeAction(let protocolVersion, let request)) = emitted else {
            return XCTFail("expected invoke_action message")
        }
        XCTAssertEqual(protocolVersion, nulangUIMessageProtocolVersion)
        XCTAssertEqual(request.documentId, "tasks")
        XCTAssertEqual(request.revision, Revision(1))
        XCTAssertEqual(request.actionId, "task.complete")
        XCTAssertEqual(request.placement, .server)
        XCTAssertFalse(request.correlationId.isEmpty)
        XCTAssertFalse(request.idempotencyKey.isEmpty)
        XCTAssertNotEqual(request.correlationId, request.idempotencyKey)
        XCTAssertEqual(request.payload, .null)
    }

    func testWrongProtocolSurfacesErrorWithoutReplacingDocument() throws {
        let store = NulangUIStore()
        store.applyRuntimeMessage(snapshot())
        let before = store.document

        store.applyRuntimeMessage(
            .snapshot(
                protocolVersion: "nulang-ui-msg/999",
                document: UiDocument(
                    documentId: "wrong",
                    revision: Revision(1),
                    root: "root",
                    nodes: [UiNode(id: "root", kind: "text")]
                )
            )
        )

        XCTAssertNotNil(store.lastError)
        XCTAssertEqual(store.document, before)
    }

    func testRendererBoundsRecursiveDepth() {
        XCTAssertTrue(NulangUIRenderLimits.allows(NulangUIRenderLimits.maximumDepth - 1))
        XCTAssertFalse(NulangUIRenderLimits.allows(NulangUIRenderLimits.maximumDepth))
    }
}
