import XCTest
@testable import NulangUIProtocol

/// Parity with nulang-ui-protocol: one event has one action per UI node.
final class ActionEventUniquenessTests: XCTestCase {
    private func binding(_ event: String, _ actionId: String) -> ActionBinding {
        ActionBinding(event: event, actionId: actionId, placement: .client)
    }

    func testDocumentRejectsDuplicateEventBindingsOnSameNode() {
        let document = UiDocument(
            documentId: "form",
            revision: Revision(1),
            root: "submit",
            nodes: [
                UiNode(
                    id: "submit",
                    kind: "button",
                    actions: [binding("activate", "save"), binding("activate", "delete")]
                )
            ]
        )

        XCTAssertThrowsError(try document.validate()) { error in
            XCTAssertEqual(
                error as? NulangUIProtocolError,
                .duplicateActionEvent(node: "submit", event: "activate")
            )
        }
    }

    func testInvalidReplaceActionsPatchDoesNotMutateDocumentOrRevision() throws {
        var document = UiDocument(
            documentId: "form",
            revision: Revision(1),
            root: "submit",
            nodes: [
                UiNode(
                    id: "submit",
                    kind: "button",
                    actions: [binding("activate", "save")]
                )
            ]
        )
        let previous = document
        let patch = UiPatch(
            documentId: "form",
            baseRevision: Revision(1),
            revision: Revision(2),
            operations: [
                .replaceActions(
                    nodeId: "submit",
                    actions: [binding("activate", "save"), binding("activate", "delete")]
                )
            ]
        )

        XCTAssertThrowsError(try document.apply(patch)) { error in
            XCTAssertEqual(
                error as? NulangUIProtocolError,
                .duplicateActionEvent(node: "submit", event: "activate")
            )
        }
        XCTAssertEqual(document, previous, "rejected patches must be atomic")
    }

    func testDifferentEventsAndDifferentNodesRemainAllowed() {
        let document = UiDocument(
            documentId: "form",
            revision: Revision(1),
            root: "root",
            nodes: [
                UiNode(id: "root", kind: "column", children: ["save", "cancel"]),
                UiNode(
                    id: "save",
                    kind: "button",
                    actions: [binding("activate", "save"), binding("focus", "record_focus")]
                ),
                UiNode(
                    id: "cancel",
                    kind: "button",
                    actions: [binding("activate", "cancel")]
                )
            ]
        )

        XCTAssertNoThrow(try document.validate())
    }
}
