import Foundation
import SwiftUI
import NulangMobileRuntime
import NulangUIHost
import NulangUIProtocol

public enum NulangAppleHostError: Error, LocalizedError {
    case unsupportedClientPayload(actionId: String)
    case serverActionHandlerMissing(actionId: String)
    case invalidDocumentCallback(String)
    case invalidRuntimeMessage(String)
    case invalidActionResult(String)

    public var errorDescription: String? {
        switch self {
        case .unsupportedClientPayload(let actionId):
            return "client action '\(actionId)' has a non-null payload; the v1 native reducer bridge currently accepts explicit form/signal snapshots only"
        case .serverActionHandlerMissing(let actionId):
            return "server action '\(actionId)' has no configured server-action handler"
        case .invalidDocumentCallback(let reason):
            return "invalid Nulang document callback: \(reason)"
        case .invalidRuntimeMessage(let reason):
            return "invalid Nulang runtime message: \(reason)"
        case .invalidActionResult(let reason):
            return "invalid Nulang client-action result: \(reason)"
        }
    }
}

/// MainActor coordinator joining the low-level native runtime to the validated
/// Swift UI store.
///
/// Native runtime work stays on `runtimeQueue`. Runtime callback bytes are
/// already copied by `NulangMobileRuntime`; this layer hops those owned bytes
/// to MainActor before mutating `NulangUIStore`. Client action execution goes
/// back to the same serialized runtime worker. Server actions never execute
/// locally and are handed to the explicit `serverActionSender` policy hook.
@MainActor
public final class NulangAppleHost: ObservableObject {
    public typealias ServerActionSender = (HostToRuntimeMessage) -> Void

    @Published public private(set) var lastError: Error?
    public let store: NulangUIStore

    private let runtime: NulangMobileRuntime
    private let runtimeQueue: DispatchQueue
    private let actionRouter: ActionRouter
    private let serverActionSender: ServerActionSender?

    private final class ActionRouter {
        var handler: ((HostToRuntimeMessage) -> Void)?

        func send(_ message: HostToRuntimeMessage) {
            handler?(message)
        }
    }

    private init(
        store: NulangUIStore,
        runtime: NulangMobileRuntime,
        runtimeQueue: DispatchQueue,
        actionRouter: ActionRouter,
        serverActionSender: ServerActionSender?
    ) {
        self.store = store
        self.runtime = runtime
        self.runtimeQueue = runtimeQueue
        self.actionRouter = actionRouter
        self.serverActionSender = serverActionSender
        self.lastError = nil
        actionRouter.handler = { [weak self] message in
            self?.route(message)
        }
    }

    /// Build the native runtime off MainActor, while creating the observable UI
    /// store on MainActor. The returned host is ready to `start()`.
    public static func create(
        artifact: Data,
        serverActionSender: ServerActionSender? = nil
    ) async throws -> NulangAppleHost {
        let actionRouter = ActionRouter()
        let store = NulangUIStore(actionSender: { message in
            actionRouter.send(message)
        })
        let runtimeQueue = DispatchQueue(
            label: "org.nulang.apple.runtime",
            qos: .userInitiated
        )

        let runtime = try await withCheckedThrowingContinuation {
            (continuation: CheckedContinuation<NulangMobileRuntime, Error>) in
            runtimeQueue.async {
                do {
                    let runtime = try NulangMobileRuntime(
                        artifact: artifact,
                        callbackQueue: runtimeQueue,
                        onDocument: { data in
                            Task { @MainActor in
                                Self.applyDocumentCallback(data, to: store)
                            }
                        },
                        onMessage: { data in
                            Task { @MainActor in
                                Self.applyRuntimeCallback(data, to: store)
                            }
                        }
                    )
                    continuation.resume(returning: runtime)
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }

        return NulangAppleHost(
            store: store,
            runtime: runtime,
            runtimeQueue: runtimeQueue,
            actionRouter: actionRouter,
            serverActionSender: serverActionSender
        )
    }

    /// Run the frozen app on the dedicated runtime worker.
    public func start() {
        lastError = nil
        let runtime = self.runtime
        runtimeQueue.async { [weak self] in
            do {
                _ = try runtime.run()
            } catch {
                Task { @MainActor [weak self] in
                    self?.lastError = error
                }
            }
        }
    }

    /// Close the native app on its serialized worker.
    public func close() {
        let runtime = self.runtime
        runtimeQueue.async {
            runtime.close()
        }
    }

    private func route(_ message: HostToRuntimeMessage) {
        do {
            try message.validate()
        } catch {
            lastError = error
            return
        }

        switch message {
        case .invokeAction(_, let request):
            switch request.placement {
            case .client:
                invokeClientAction(request)
            case .server:
                guard let serverActionSender else {
                    lastError = NulangAppleHostError.serverActionHandlerMissing(
                        actionId: request.actionId
                    )
                    return
                }
                serverActionSender(message)
            }
        }
    }

    private func invokeClientAction(_ request: ActionRequest) {
        // v1 proves the authorization/execution vertical slice without silently
        // discarding semantic payload. Payload-bearing actions remain blocked
        // until the reducer request protocol carries that field explicitly.
        guard request.payload == .null else {
            lastError = NulangAppleHostError.unsupportedClientPayload(
                actionId: request.actionId
            )
            return
        }

        let invocation: [String: Any] = [
            "protocol": "nulang-action-invoke/1",
            "handler": request.actionId,
            "correlation_id": request.correlationId,
            "idempotency_key": request.idempotencyKey,
            "form": [String: String](),
            "signals": [String: String](),
        ]

        let requestData: Data
        do {
            requestData = try JSONSerialization.data(
                withJSONObject: invocation,
                options: [.sortedKeys]
            )
        } catch {
            lastError = error
            return
        }

        lastError = nil
        let runtime = self.runtime
        let expectedCorrelation = request.correlationId
        runtimeQueue.async { [weak self] in
            do {
                let result = try runtime.invokeAction(requestJSON: requestData)
                Task { @MainActor [weak self] in
                    self?.applyActionResult(
                        result,
                        expectedCorrelation: expectedCorrelation
                    )
                }
            } catch {
                Task { @MainActor [weak self] in
                    self?.lastError = error
                }
            }
        }
    }

    private static func applyDocumentCallback(
        _ data: Data,
        to store: NulangUIStore
    ) {
        do {
            // The dedicated document callback is allowed to deliver the bare
            // nulang-ui/1 document. Normalize it into the frozen host snapshot
            // message before touching store state.
            let document = try JSONDecoder().decode(UiDocument.self, from: data)
            try document.validate()
            store.applyRuntimeMessage(
                .snapshot(
                    protocolVersion: nulangUIMessageProtocolVersion,
                    document: document
                )
            )
            return
        } catch {
            // Some runtimes may already emit a full RuntimeToHostMessage on the
            // document channel. Accept that exact frozen shape as a fallback.
            do {
                store.applyRuntimeMessage(try NulangUICodec.decodeRuntimeMessage(data))
            } catch {
                // `NulangUIStore` intentionally owns protocol-state errors, but
                // decode failures happen before it receives a typed message.
                // Leave the last valid document intact.
            }
        }
    }

    private static func applyRuntimeCallback(
        _ data: Data,
        to store: NulangUIStore
    ) {
        do {
            store.applyRuntimeMessage(try NulangUICodec.decodeRuntimeMessage(data))
        } catch {
            // Preserve the last valid document. The generated coordinator owns
            // user-visible integration errors; raw callback decode failure does
            // not mutate store state.
        }
    }

    private func applyActionResult(
        _ data: Data,
        expectedCorrelation: String
    ) {
        do {
            guard
                let object = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                object["protocol"] as? String == "nulang-action-result/1",
                let correlation = object["correlation_id"] as? String,
                correlation == expectedCorrelation,
                let messages = object["messages"] as? [[String: Any]]
            else {
                throw NulangAppleHostError.invalidActionResult(
                    "result envelope or correlation is invalid"
                )
            }

            for envelope in messages {
                guard
                    let protocolVersion = envelope["protocol"] as? String,
                    protocolVersion == nulangUIMessageProtocolVersion,
                    var message = envelope["message"] as? [String: Any]
                else {
                    throw NulangAppleHostError.invalidActionResult(
                        "message envelope is not nulang-ui-msg/1"
                    )
                }

                // Native action results carry `{protocol,message}` envelopes;
                // NulangUIProtocol decodes the flattened frozen runtime message.
                message["protocol"] = protocolVersion
                let messageData = try JSONSerialization.data(
                    withJSONObject: message,
                    options: [.sortedKeys]
                )
                store.applyRuntimeMessage(
                    try NulangUICodec.decodeRuntimeMessage(messageData)
                )
            }
            lastError = nil
        } catch {
            lastError = error
        }
    }
}
