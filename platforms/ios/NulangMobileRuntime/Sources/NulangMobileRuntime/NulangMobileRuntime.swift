import Foundation
import CNulangMobile

public enum NulangMobileRuntimeError: Error, Equatable {
    case create(String)
    case run(String)
    case closed
}

/// Thin Swift owner for the shared C mobile runtime.
///
/// This layer is deliberately UI- and protocol-neutral. It owns the native
/// app handle, serializes access to it, copies callback bytes before the C
/// callback returns, and forwards those bytes to caller-selected queues.
/// Semantic UI decoding and MainActor publication live in NulangUIHost.
public final class NulangMobileRuntime: @unchecked Sendable {
    public typealias JSONHandler = @Sendable (Data) -> Void

    private let lock = NSLock()
    private let callbackQueue: DispatchQueue
    private let onDocument: JSONHandler
    private let onMessage: JSONHandler
    private var app: OpaquePointer?

    private static let documentCallback:
        @convention(c) (UnsafePointer<CChar>?, UnsafeMutableRawPointer?) -> Void =
    { json, context in
        guard let json, let context else { return }
        let runtime = Unmanaged<NulangMobileRuntime>
            .fromOpaque(context)
            .takeUnretainedValue()
        runtime.deliver(json, to: runtime.onDocument)
    }

    private static let messageCallback:
        @convention(c) (UnsafePointer<CChar>?, UnsafeMutableRawPointer?) -> Void =
    { json, context in
        guard let json, let context else { return }
        let runtime = Unmanaged<NulangMobileRuntime>
            .fromOpaque(context)
            .takeUnretainedValue()
        runtime.deliver(json, to: runtime.onMessage)
    }

    public init(
        artifact: Data,
        callbackQueue: DispatchQueue = .main,
        onDocument: @escaping JSONHandler,
        onMessage: @escaping JSONHandler
    ) throws {
        guard !artifact.isEmpty else {
            throw NulangMobileRuntimeError.create("Nulang .nbc artifact is empty")
        }

        self.callbackQueue = callbackQueue
        self.onDocument = onDocument
        self.onMessage = onMessage
        self.app = nil

        let callbacks = NulangMobileCallbacks(
            document: Self.documentCallback,
            message: Self.messageCallback,
            context: Unmanaged.passUnretained(self).toOpaque()
        )
        var created: OpaquePointer?
        var error = [CChar](repeating: 0, count: 512)

        let status = artifact.withUnsafeBytes { raw -> NulangMobileStatus in
            let bytes = raw.bindMemory(to: UInt8.self)
            return error.withUnsafeMutableBufferPointer { errorBuffer in
                nulang_mobile_app_new(
                    bytes.baseAddress,
                    bytes.count,
                    callbacks,
                    &created,
                    errorBuffer.baseAddress,
                    errorBuffer.count
                )
            }
        }

        guard status.rawValue == NULANG_MOBILE_OK.rawValue, let created else {
            throw NulangMobileRuntimeError.create(Self.errorText(error))
        }
        self.app = created
    }

    deinit {
        close()
    }

    /// Execute the frozen application module.
    ///
    /// This method is synchronous. Higher-level hosts should call it from a
    /// dedicated runtime worker rather than from MainActor.
    @discardableResult
    public func run() throws -> UInt64 {
        try synchronized {
            guard let app else {
                throw NulangMobileRuntimeError.closed
            }

            var value = NulangValue(raw: 0)
            var error = [CChar](repeating: 0, count: 512)
            let status = error.withUnsafeMutableBufferPointer { buffer in
                nulang_mobile_app_run(
                    app,
                    &value,
                    buffer.baseAddress,
                    buffer.count
                )
            }
            guard status.rawValue == NULANG_MOBILE_OK.rawValue else {
                throw NulangMobileRuntimeError.run(Self.errorText(error))
            }
            return value.raw
        }
    }

    public func close() {
        synchronized {
            guard let app else { return }
            self.app = nil
            nulang_mobile_app_free(app)
        }
    }

    private func synchronized<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }

    private func deliver(
        _ json: UnsafePointer<CChar>,
        to handler: @escaping JSONHandler
    ) {
        // The native callback's pointer is borrowed only for this call. Copy
        // into Swift-owned bytes synchronously before returning to C.
        let data = Data(String(cString: json).utf8)
        callbackQueue.async {
            handler(data)
        }
    }

    private static func errorText(_ buffer: [CChar]) -> String {
        buffer.withUnsafeBufferPointer { pointer in
            guard let base = pointer.baseAddress else {
                return "unknown Nulang runtime error"
            }
            let message = String(cString: base)
            return message.isEmpty ? "unknown Nulang runtime error" : message
        }
    }
}
