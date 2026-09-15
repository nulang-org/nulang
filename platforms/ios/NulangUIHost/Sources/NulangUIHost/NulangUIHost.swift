import Foundation
import SwiftUI
import NulangUIProtocol

@MainActor
public final class NulangUIStore: ObservableObject {
    public typealias ActionSender = (HostToRuntimeMessage) -> Void

    @Published public private(set) var document: UiDocument?
    @Published public private(set) var lastError: Error?

    private var runtimeState = UiRuntimeState()
    private let actionSender: ActionSender?

    public init(actionSender: ActionSender? = nil) {
        self.actionSender = actionSender
    }

    public func applyRuntimeJSON(_ json: String) {
        do {
            var next = runtimeState
            try next.apply(json: json)
            runtimeState = next
            document = next.document
            lastError = nil
        } catch {
            lastError = error
        }
    }

    public func applyRuntimeMessage(_ message: RuntimeToHostMessage) {
        do {
            var next = runtimeState
            try next.apply(message)
            runtimeState = next
            document = next.document
            lastError = nil
        } catch {
            lastError = error
        }
    }

    public func sendAction(
        nodeId: String,
        event: String,
        payload: WireValue = .null
    ) {
        guard
            let document,
            let node = document.node(id: nodeId),
            let binding = node.actions.first(where: { $0.event == event })
        else {
            return
        }
        send(binding: binding, document: document, payload: payload)
    }

    public func sendFirstAction(
        nodeId: String,
        preferredEvents: [String],
        payload: WireValue = .null
    ) {
        guard let document, let node = document.node(id: nodeId) else {
            return
        }

        let binding = preferredEvents
            .compactMap { event in node.actions.first(where: { $0.event == event }) }
            .first ?? node.actions.first
        guard let binding else {
            return
        }
        send(binding: binding, document: document, payload: payload)
    }

    private func send(
        binding: ActionBinding,
        document: UiDocument,
        payload: WireValue
    ) {
        let correlationId = UUID().uuidString.lowercased()
        let idempotencyKey = UUID().uuidString.lowercased()
        let request = ActionRequest(
            documentId: document.documentId,
            revision: document.revision,
            actionId: binding.actionId,
            placement: binding.placement,
            correlationId: correlationId,
            idempotencyKey: idempotencyKey,
            payload: payload
        )
        actionSender?(
            .invokeAction(
                protocolVersion: nulangUIMessageProtocolVersion,
                request: request
            )
        )
    }
}

enum NulangUIRenderLimits {
    /// SwiftUI view construction below this layer is recursive. Keep that
    /// recursion bounded even though protocol validation intentionally accepts
    /// much deeper acyclic documents using an iterative traversal.
    static let maximumDepth = 256

    static func allows(_ depth: Int) -> Bool {
        depth < maximumDepth
    }
}

public struct NulangUIRootView: View {
    @ObservedObject private var store: NulangUIStore

    public init(store: NulangUIStore) {
        self.store = store
    }

    public var body: some View {
        Group {
            if let document = store.document,
               let root = document.node(id: document.root) {
                render(root, in: document, depth: 0)
            } else if let error = store.lastError {
                Text(error.localizedDescription)
                    .accessibilityIdentifier("nulang-ui-error")
            } else {
                ProgressView()
                    .accessibilityIdentifier("nulang-ui-loading")
            }
        }
    }

    private func render(_ node: UiNode, in document: UiDocument, depth: Int) -> AnyView {
        guard NulangUIRenderLimits.allows(depth) else {
            return AnyView(
                Text("Nulang UI render depth limit exceeded")
                    .font(.caption)
                    .accessibilityIdentifier("nulang-ui-depth-limit-\(node.id)")
            )
        }

        switch node.kind {
        case "column":
            return AnyView(
                VStack(
                    alignment: horizontalAlignment(node.properties["alignment"]),
                    spacing: spacing(node.properties["spacing"])
                ) {
                    renderChildren(node, in: document, depth: depth)
                }
            )

        case "row":
            return AnyView(
                HStack(
                    alignment: verticalAlignment(node.properties["alignment"]),
                    spacing: spacing(node.properties["spacing"])
                ) {
                    renderChildren(node, in: document, depth: depth)
                }
            )

        case "text":
            return AnyView(
                Text(node.properties["text"]?.stringValue ?? "")
                    .accessibilityIdentifier(node.id)
            )

        case "button":
            let label = node.properties["label"]?.stringValue
                ?? node.properties["text"]?.stringValue
                ?? "Button"
            return AnyView(
                Button(label) {
                    store.sendFirstAction(
                        nodeId: node.id,
                        preferredEvents: ["press", "tap", "click"]
                    )
                }
                .accessibilityIdentifier(node.id)
            )

        case "spacer":
            return AnyView(Spacer())

        case "divider":
            return AnyView(Divider())

        default:
            return AnyView(
                Text("Unsupported Nulang UI node: \(node.kind)")
                    .font(.caption)
                    .accessibilityIdentifier("unsupported-\(node.id)")
            )
        }
    }

    @ViewBuilder
    private func renderChildren(_ node: UiNode, in document: UiDocument, depth: Int) -> some View {
        ForEach(node.children, id: \.self) { childId in
            if let child = document.node(id: childId) {
                render(child, in: document, depth: depth + 1)
            }
        }
    }

    private func spacing(_ value: WireValue?) -> CGFloat? {
        guard case .i64(let integer) = value else {
            return nil
        }
        return CGFloat(integer.rawValue)
    }

    private func horizontalAlignment(_ value: WireValue?) -> HorizontalAlignment {
        switch value?.stringValue {
        case "center": return .center
        case "trailing", "end": return .trailing
        default: return .leading
        }
    }

    private func verticalAlignment(_ value: WireValue?) -> VerticalAlignment {
        switch value?.stringValue {
        case "top", "start": return .top
        case "bottom", "end": return .bottom
        default: return .center
        }
    }
}
