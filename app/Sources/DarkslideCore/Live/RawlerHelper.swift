import DarkslideFFI
import Foundation
import Observation

/// The rawler helper (`crates/ds-rawler`, docs/RAWDEC.md): `darkslide-rawler`, a separate
/// program that runs the LGPL-2.1 rawler decoder for what Darkslide's own decoders don't
/// cover (CR3, RAF, ORF, RW2, the colour data of non-DNG cameras, HDR and panorama DNGs).
/// Darkslide doesn't contain it. The user downloads it (Settings ▸ RAW, or the viewer's
/// banner): the pinned GitHub release asset, checked by SHA-256, installed as
/// `<support>/Helpers/darkslide-rawler` in one rename. The Rust core then runs it for
/// those files. `DS_RAWLER_HELPER` names another one (development, tests).
@MainActor @Observable
public final class RawlerHelper {
    public enum State: Equatable, Sendable {
        case notInstalled
        case downloading(Double)
        /// In use: the rawler version it links.
        case installed(String)
        /// Downloaded or installed, but it doesn't run or the download failed: why.
        case failed(String)
    }

    /// The pinned download (`scripts/build-rawler-helper.sh` prints the hash and size).
    public struct Release: Sendable, Equatable {
        public var url: URL
        public var sha256: String
        public var size: Int64
    }

    /// The helper published with this build. A new helper is a new release tag here.
    public nonisolated static let release = Release(
        url: URL(string: "https://github.com/sbaillar/darkslide/releases/download/rawler-helper-1/darkslide-rawler-macos-arm64")!,
        sha256: "b5dec0a5c9000bd8b68359528e3fd13b8abe0b9d13bf2aed2538f329de8812cc",
        size: 5_787_312)

    /// The app's, in `<support>/Helpers`.
    public static let shared = RawlerHelper(root: AppPaths.support.appendingPathComponent("Helpers", isDirectory: true))

    public private(set) var state: State = .notInstalled
    @ObservationIgnored let root: URL
    @ObservationIgnored let release: Release
    @ObservationIgnored private let downloader: any ModelDownloader
    /// Tells the core which helper to run (`nil`: none); returns its rawler version.
    @ObservationIgnored private let use: @Sendable (String?) throws -> String?
    @ObservationIgnored private var running: Task<Void, Error>?

    public init(root: URL, release: Release = RawlerHelper.release,
                downloader: any ModelDownloader = URLSessionModelDownloader(),
                use: @escaping @Sendable (String?) throws -> String? = { try setRawlerHelper(path: $0) }) {
        self.root = root
        self.release = release
        self.downloader = downloader
        self.use = use
    }

    /// Where the helper is installed.
    public var executable: URL { root.appendingPathComponent("darkslide-rawler") }

    public var isDownloading: Bool {
        if case .downloading = state { return true }
        return false
    }

    /// Whether a develop failure's reason is the core asking for the helper.
    public nonisolated static func isNeeded(_ reason: String) -> Bool { reason.contains("rawler helper") }

    /// Use the installed helper, if there is one (at launch, and after an install). The
    /// core asks it who it is first (one short process run).
    public func activate() {
        guard FileManager.default.isExecutableFile(atPath: executable.path) else {
            state = .notInstalled
            return
        }
        do {
            state = .installed(try use(executable.path) ?? "")
        } catch {
            state = .failed(Self.message(error))
        }
    }

    /// Fetch, check, install and use the helper (one run at a time). After `cancel()`
    /// this throws `CancellationError` and an earlier install is left as it was.
    public func download() async throws {
        if let running { return try await running.value }
        state = .downloading(0)
        let task = Task { try await self.fetch() }
        running = task
        defer { running = nil }
        do {
            try await task.value
        } catch {
            if error is CancellationError || task.isCancelled {
                activate()
                throw CancellationError()
            }
            state = .failed(Self.message(error))
            throw error
        }
        activate()
        if case .failed(let m) = state { throw RawlerHelperError(message: m) }
    }

    public func cancel() {
        running?.cancel()
    }

    /// Stop using the helper and delete it (a program Darkslide downloaded, never a
    /// photo): files that need it go back to their embedded previews.
    public func remove() {
        _ = try? use(nil)
        try? FileManager.default.removeItem(at: executable)
        state = .notInstalled
    }

    private func fetch() async throws {
        let fm = FileManager.default
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let staging = root.appendingPathComponent(".darkslide-rawler-\(UUID().uuidString)")
        defer { try? fm.removeItem(at: staging) }
        let total = Double(max(release.size, 1))
        do {
            try await ModelFiles.fetch(release.url, to: staging, sha256: release.sha256, size: release.size,
                                       path: "darkslide-rawler", using: downloader, progress: { [weak self] bytes in
                let f = min(1, Double(bytes) / total)
                Task { @MainActor in if case .downloading = self?.state { self?.state = .downloading(f) } }
            })
        } catch let e as ModelFetchError {
            switch e {
            case .download(let m): throw RawlerHelperError(message: "The RAW helper couldn’t be downloaded: \(m)")
            case .checksum: throw RawlerHelperError(message: "The downloaded RAW helper isn’t the expected file (checksum mismatch).")
            }
        }
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: staging.path)
        try Task.checkCancellation()
        if fm.fileExists(atPath: executable.path) {
            _ = try fm.replaceItemAt(executable, withItemAt: staging)
        } else {
            try fm.moveItem(at: staging, to: executable)
        }
    }

    static func message(_ error: Error) -> String {
        if let e = error as? DsError { return RenderError(e).message }
        return (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }
}

public struct RawlerHelperError: Error, Equatable, LocalizedError {
    public var message: String
    public var errorDescription: String? { message }
}

/// What Settings ▸ RAW and the viewer's banner say about the helper.
public enum RawlerHelperText {
    /// The download's size, e.g. "5.8 MB".
    public static var size: String {
        ByteCountFormatter.string(fromByteCount: RawlerHelper.release.size, countStyle: .file)
    }

    public static func status(_ state: RawlerHelper.State) -> String {
        switch state {
        case .notInstalled: "Not installed"
        case .downloading(let f): "Downloading… \(Int((f * 100).rounded()))%"
        case .installed(let v): v.isEmpty ? "Installed" : "Installed (rawler \(v))"
        case .failed(let m): m
        }
    }
}
