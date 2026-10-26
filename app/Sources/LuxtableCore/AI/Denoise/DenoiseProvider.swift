import LuxtableFFI
import Foundation

/// The app's AI Denoise for Rust's export (`set_denoise_provider`): Rust calls `ensure`
/// from an export worker just before a photo renders, and this blocks that worker until
/// the photo is denoised. A failure fails that photo, not the export; cancelling the
/// export (its token) cancels the denoise within ~50 ms.
final class AppDenoiseProvider: DenoiseProvider, @unchecked Sendable {
    // Immutable; `ensure` keeps its per-call state local.
    let ensureOne: @Sendable (URL) async throws -> Bool

    init(_ ensureOne: @escaping @Sendable (URL) async throws -> Bool) {
        self.ensureOne = ensureOne
    }

    /// Make it Rust's provider for exports.
    static func install(_ ensureOne: @escaping @Sendable (URL) async throws -> Bool) {
        setDenoiseProvider(provider: AppDenoiseProvider(ensureOne))
    }

    /// Rust's exports have no provider (tests).
    static func uninstall() {
        setDenoiseProvider(provider: nil)
    }

    func ensure(path: String, cancel: CancelToken) throws -> Bool {
        let done = DispatchSemaphore(value: 0)
        let outcome = Outcome()
        let work = ensureOne
        let task = Task.detached {
            do {
                outcome.set(.success(try await work(URL(fileURLWithPath: path))))
            } catch {
                outcome.set(.failure(error))
            }
            done.signal()
        }
        while done.wait(timeout: .now() + .milliseconds(50)) == .timedOut {
            if (try? cancel.isCancelled()) == true { task.cancel() }
        }
        switch outcome.result {
        case .success(let available): return available
        case .failure(let e) where e is CancellationError: throw DsError.Cancelled
        case .failure(let e): throw DsError.Unsupported(message: (e as? LocalizedError)?.errorDescription ?? "\(e)")
        case nil: throw DsError.Internal(message: "AI Denoise ended without a result")
        }
    }

    private final class Outcome: @unchecked Sendable {
        private let lock = NSLock()
        private var stored: Result<Bool, Error>?
        func set(_ r: Result<Bool, Error>) { lock.withLock { stored = r } }
        var result: Result<Bool, Error>? { lock.withLock { stored } }
    }
}
