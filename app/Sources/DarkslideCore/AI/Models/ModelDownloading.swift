import CoreML
import CryptoKit
import Foundation

/// Downloads one file, reporting bytes as they arrive (seam for tests).
public protocol ModelDownloader: Sendable {
    func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws
}

/// URLSession download straight to a file (no per-byte loop), with progress from the
/// task's `Progress`; cancelling the calling task cancels the download. HTTPS only, and a
/// redirect is followed only to another https URL (local copies go through
/// `LocalModelDownloader` / `DirectoryModelDownloader`).
public struct URLSessionModelDownloader: ModelDownloader {
    /// Allowed URL schemes; tests add "file" to exercise the URLSession path offline.
    let schemes: Set<String>

    public init() { schemes = ["https"] }

    init(schemes: Set<String>) { self.schemes = schemes }

    public func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
        guard schemes.contains(url.scheme?.lowercased() ?? "") else {
            throw SegmentationError.download("only https downloads are allowed (\(url.lastPathComponent))")
        }
        let running = RunningDownload()
        let size: Int64 = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (c: CheckedContinuation<Int64, Error>) in
                let task = URLSession.shared.downloadTask(with: url) { file, response, error in
                    running.finish()
                    c.resume(with: Result {
                        if let error {
                            if (error as? URLError)?.code == .cancelled { throw CancellationError() }
                            throw error
                        }
                        if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
                            throw SegmentationError.download("HTTP \(http.statusCode) for \(url.lastPathComponent)")
                        }
                        guard let file else { throw SegmentationError.download(url.lastPathComponent) }
                        // The temporary file is removed when this handler returns: move it now.
                        let fm = FileManager.default
                        if fm.fileExists(atPath: destination.path) { try fm.removeItem(at: destination) }
                        try fm.moveItem(at: file, to: destination)
                        return (try fm.attributesOfItem(atPath: destination.path)[.size] as? NSNumber)?.int64Value ?? 0
                    })
                }
                task.delegate = HTTPSRedirectsOnly.shared
                running.start(task, progress: progress)
            }
        } onCancel: {
            running.cancel()
        }
        progress(size)
    }
}

/// Follows a redirect only to an https URL; otherwise the 3xx completes the task (and fails it).
final class HTTPSRedirectsOnly: NSObject, URLSessionTaskDelegate, Sendable {
    static let shared = HTTPSRedirectsOnly()

    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
        completionHandler(request.url?.scheme?.lowercased() == "https" ? request : nil)
    }
}

/// One download task, cancellable from any thread (also before it has started).
private final class RunningDownload: @unchecked Sendable {
    private let lock = NSLock()
    private var task: URLSessionDownloadTask?
    private var observation: NSKeyValueObservation?
    private var cancelled = false

    func start(_ task: URLSessionDownloadTask, progress: @escaping @Sendable (Int64) -> Void) {
        let observation = task.progress.observe(\.completedUnitCount) { p, _ in progress(p.completedUnitCount) }
        let go = lock.withLock {
            self.task = task
            self.observation = observation
            return !cancelled
        }
        if go { task.resume() } else { task.cancel() }
    }

    func cancel() {
        let task = lock.withLock {
            cancelled = true
            return self.task
        }
        task?.cancel()
    }

    func finish() {
        let o = lock.withLock {
            defer { observation = nil }
            return observation
        }
        o?.invalidate()
    }
}

/// "Downloads" from a local copy of the package (the part of the URL after `.mlpackage/`).
public struct LocalModelDownloader: ModelDownloader {
    public let package: URL
    public init(package: URL) { self.package = package }

    public func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
        progress(try Self.copy(url, package: package, to: destination))
    }

    /// Copy the package file `url` names to `destination`; returns its size.
    static func copy(_ url: URL, package: URL, to destination: URL) throws -> Int64 {
        guard let r = url.path.range(of: ".mlpackage/") else { throw SegmentationError.download(url.lastPathComponent) }
        let src = package.appendingPathComponent(String(url.path[r.upperBound...]))
        let fm = FileManager.default
        if fm.fileExists(atPath: destination.path) { try? fm.removeItem(at: destination) }
        do {
            try fm.copyItem(at: src, to: destination)
            let size = try fm.attributesOfItem(atPath: destination.path)[.size] as? NSNumber
            return size?.int64Value ?? 0
        } catch {
            throw SegmentationError.download(error.localizedDescription)
        }
    }
}

/// Why `ModelFiles.fetch` gave up on one file; each model maps it onto its own error.
public enum ModelFetchError: Error, Equatable {
    case download(String)
    case checksum(String)
}

/// What every model download shares: fetching one verified file, hashing, compiling and
/// staging-folder housekeeping. A plain enum on purpose: going through a static of a
/// main-actor class from a detached task aborted in the Swift 6.2 task allocator.
public enum ModelFiles {
    /// Allowed overshoot past a file's pinned size before a download is cut off.
    static let sizeSlack: Int64 = 1 << 20

    /// Streaming SHA-256 (hex).
    public static func sha256(of url: URL) throws -> String {
        let h = try FileHandle(forReadingFrom: url)
        defer { try? h.close() }
        var d = SHA256()
        while let chunk = try h.read(upToCount: 1 << 20), !chunk.isEmpty { d.update(data: chunk) }
        return d.finalize().map { String(format: "%02x", $0) }.joined()
    }

    /// Compiles with the synchronous API on a GCD queue, off the main actor.
    @Sendable public static func compile(_ package: URL) async throws -> URL {
        try await withCheckedThrowingContinuation { c in
            DispatchQueue.global(qos: .userInitiated).async {
                c.resume(with: Result { try MLModel.compileModel(at: package) })
            }
        }
    }

    /// Downloads `url` to `destination` and checks it, hashing off the calling actor. The
    /// download is cut off once it passes `size` (plus slack), so a hostile host can't fill
    /// the disk before the hash is looked at. `check` replaces the SHA-256 comparison (for
    /// a file with no pinned hash). `CancellationError` passes through unchanged.
    public static func fetch(_ url: URL, to destination: URL, sha256 pinned: String, size: Int64, path: String,
                             using downloader: any ModelDownloader,
                             progress: @escaping @Sendable (Int64) -> Void,
                             check: (@Sendable (URL) throws -> Bool)? = nil) async throws {
        let limit = size + sizeSlack
        let overflow = Overflow()
        let transfer = Task {
            try await downloader.download(url, to: destination) { bytes in
                progress(bytes)
                if bytes > limit { overflow.trip() }
            }
        }
        overflow.task = transfer
        do {
            try await withTaskCancellationHandler { try await transfer.value } onCancel: { transfer.cancel() }
        } catch is CancellationError {
            if overflow.tripped { throw ModelFetchError.download("\(path) is larger than expected") }
            throw CancellationError()
        } catch let e as SegmentationError {
            if case .download(let m) = e { throw ModelFetchError.download(m) }
            throw ModelFetchError.download(e.localizedDescription)
        } catch {
            throw ModelFetchError.download((error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
        }
        let actual = (try? FileManager.default.attributesOfItem(atPath: destination.path)[.size] as? NSNumber)?.int64Value ?? 0
        guard actual <= limit else { throw ModelFetchError.download("\(path) is larger than expected") }
        let ok = try await Task.detached(priority: .userInitiated) {
            if let check { return try check(destination) }
            return try sha256(of: destination) == pinned
        }.value
        guard ok else { throw ModelFetchError.checksum(path) }
    }

    /// Removes the staging folders `<root>/<prefix>*` that a crash or quit left behind. Each
    /// model sweeps only its own prefix, so another model's download in flight is never taken.
    public static func sweepStaging(in root: URL, prefix: String) {
        let fm = FileManager.default
        for name in (try? fm.contentsOfDirectory(atPath: root.path)) ?? [] where name.hasPrefix(prefix) {
            try? fm.removeItem(at: root.appendingPathComponent(name))
        }
    }

    /// Staging folders of builds that shared one `.download-<uuid>` prefix: nobody owns them any
    /// more, so those untouched for `age` seconds (a crash's leftovers, never a live download) go.
    /// Only that exact shape: another model's `.download-<name>` in flight is never taken.
    public static func sweepLegacyStaging(in root: URL, olderThan age: TimeInterval = 3600) {
        let fm = FileManager.default
        let now = Date()
        for name in (try? fm.contentsOfDirectory(atPath: root.path)) ?? []
        where name.hasPrefix(".download-") && UUID(uuidString: String(name.dropFirst(".download-".count))) != nil {
            let url = root.appendingPathComponent(name)
            let touched = (try? fm.attributesOfItem(atPath: url.path)[.modificationDate] as? Date) ?? .distantPast
            if now.timeIntervalSince(touched) > age { try? fm.removeItem(at: url) }
        }
    }

    /// Lets the byte-progress callback cancel the transfer that is reporting it.
    private final class Overflow: @unchecked Sendable {
        private let lock = NSLock()
        private var _task: Task<Void, Error>?
        private var _tripped = false

        var task: Task<Void, Error>? {
            get { lock.withLock { _task } }
            set {
                let late = lock.withLock { () -> Bool in
                    _task = newValue
                    return _tripped
                }
                if late { newValue?.cancel() } // the callback tripped before the task was recorded
            }
        }

        var tripped: Bool { lock.withLock { _tripped } }

        func trip() {
            let t = lock.withLock { () -> Task<Void, Error>? in
                _tripped = true
                return _task
            }
            t?.cancel()
        }
    }
}
