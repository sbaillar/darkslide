import LuxtableFFI
import Foundation
import Testing
@testable import LuxtableCore

/// The rawler helper's download, install, use and removal (`RawlerHelper`), and how the
/// viewer offers it.
@MainActor
@Suite(.serialized) struct RawlerHelperTests {
    /// Serves `data` for any URL, reporting progress halfway and at the end.
    struct Serves: ModelDownloader {
        let data: Data
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            try data.write(to: destination)
            progress(Int64(data.count) / 2)
            progress(Int64(data.count))
        }
    }

    struct Stalls: ModelDownloader {
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            progress(1)
            while true { try await Task.sleep(for: .milliseconds(5)) }
        }
    }

    /// Records what the core was told to use; answers with a version or fails.
    final class Core: @unchecked Sendable {
        private let lock = NSLock()
        private var _paths: [String?] = []
        let fails: Bool
        init(fails: Bool = false) { self.fails = fails }
        var paths: [String?] { lock.withLock { _paths } }

        func use(_ path: String?) throws -> String? {
            lock.withLock { _paths.append(path) }
            if fails, path != nil { throw RawlerHelperError(message: "it doesn’t run") }
            return path == nil ? nil : "0.8.0"
        }
    }

    static let program = Data("#!/bin/sh\n".utf8)

    static func root() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("lt-rawler-\(UUID())", isDirectory: true)
    }

    static func release(sha: String? = nil) throws -> RawlerHelper.Release {
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("lt-rawler-sha-\(UUID())")
        try program.write(to: tmp)
        defer { try? FileManager.default.removeItem(at: tmp) }
        return RawlerHelper.Release(url: URL(string: "https://example.invalid/luxtable-rawler")!,
                                    sha256: try sha ?? ModelFiles.sha256(of: tmp), size: Int64(program.count))
    }

    static func helper(root: URL, core: Core, downloader: any ModelDownloader = Serves(data: program),
                       sha: String? = nil) throws -> RawlerHelper {
        RawlerHelper(root: root, release: try release(sha: sha), downloader: downloader, use: { try core.use($0) })
    }

    @Test func downloadInstallsAnExecutableAndUsesIt() async throws {
        let root = Self.root()
        defer { try? FileManager.default.removeItem(at: root) }
        let core = Core()
        let h = try Self.helper(root: root, core: core)
        h.activate()
        #expect(h.state == .notInstalled)
        #expect(core.paths.isEmpty, "nothing installed: the core isn't asked")
        try await h.download()
        #expect(h.state == .installed("0.8.0"))
        #expect(FileManager.default.isExecutableFile(atPath: h.executable.path))
        #expect(core.paths == [h.executable.path])
        // No staging file is left beside it.
        #expect(try FileManager.default.contentsOfDirectory(atPath: root.path) == ["luxtable-rawler"])
        // A second download replaces it in place.
        try await h.download()
        #expect(try Data(contentsOf: h.executable) == Self.program)
    }

    @Test func aWrongChecksumInstallsNothing() async throws {
        let root = Self.root()
        defer { try? FileManager.default.removeItem(at: root) }
        let h = try Self.helper(root: root, core: Core(), sha: String(repeating: "0", count: 64))
        await #expect(throws: RawlerHelperError.self) { try await h.download() }
        guard case .failed(let m) = h.state else { Issue.record("\(h.state)"); return }
        #expect(m.contains("checksum"))
        #expect(!FileManager.default.fileExists(atPath: h.executable.path))
    }

    @Test func aDownloadThatFailsSaysWhy() async throws {
        let root = Self.root()
        defer { try? FileManager.default.removeItem(at: root) }
        struct Fails: ModelDownloader {
            func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
                throw SegmentationError.download("HTTP 404 for luxtable-rawler")
            }
        }
        let h = try Self.helper(root: root, core: Core(), downloader: Fails())
        await #expect(throws: RawlerHelperError.self) { try await h.download() }
        #expect(RawlerHelperText.status(h.state) == "The RAW helper couldn’t be downloaded: HTTP 404 for luxtable-rawler")
    }

    @Test func aHelperThatDoesntRunIsAFailureNotAnInstall() async throws {
        let root = Self.root()
        defer { try? FileManager.default.removeItem(at: root) }
        let h = try Self.helper(root: root, core: Core(fails: true))
        await #expect(throws: RawlerHelperError.self) { try await h.download() }
        #expect(h.state == .failed("it doesn’t run"))
        // At the next launch the same: shown, not hidden.
        h.activate()
        #expect(h.state == .failed("it doesn’t run"))
    }

    @Test func cancellingLeavesNothingInstalled() async throws {
        let root = Self.root()
        defer { try? FileManager.default.removeItem(at: root) }
        let h = try Self.helper(root: root, core: Core(), downloader: Stalls())
        let run = Task { try await h.download() }
        #expect(await waitUntil { h.isDownloading })
        h.cancel()
        await #expect(throws: CancellationError.self) { try await run.value }
        #expect(h.state == .notInstalled)
        #expect(!FileManager.default.fileExists(atPath: h.executable.path))
    }

    @Test func removeStopsUsingItAndDeletesIt() async throws {
        let root = Self.root()
        defer { try? FileManager.default.removeItem(at: root) }
        let core = Core()
        let h = try Self.helper(root: root, core: core)
        try await h.download()
        h.remove()
        #expect(h.state == .notInstalled)
        #expect(!FileManager.default.fileExists(atPath: h.executable.path))
        #expect(core.paths.last == .some(nil))
    }

    /// The real core with the helper the test run built (`LT_RAWLER_HELPER`, set by
    /// scripts/test-app.sh): installed under another root, it answers with its version.
    @Test func theCoreChecksARealHelper() throws {
        guard let built = LaunchEnvironment.value("LT_RAWLER_HELPER"), !built.isEmpty else { return }
        let root = Self.root()
        defer { try? FileManager.default.removeItem(at: root) }
        let h = RawlerHelper(root: root)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try FileManager.default.copyItem(at: URL(fileURLWithPath: built), to: h.executable)
        h.activate()
        guard case .installed(let v) = h.state else { Issue.record("\(h.state)"); return }
        #expect(!v.isEmpty)
        h.remove() // back to the environment's helper
        // Something that isn't a helper is refused, and the one in use stays.
        let bogus = root.appendingPathComponent("bogus")
        try Data("#!/bin/sh\nexit 1\n".utf8).write(to: bogus)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: bogus.path)
        #expect(throws: DsError.self) { try setRawlerHelper(path: bogus.path) }
    }

    @Test func theViewerOffersTheHelperOnlyWhenItIsWhatsMissing() {
        let needs = ViewerProblem.cannotDevelop(
            "unsupported camera SONY ILCE-9: no colour matrix for this camera: needs the rawler helper, which isn't installed")
        #expect(needs.needsRawlerHelper)
        #expect(needs.actions.map(\.action) == [.getRawlerHelper])
        #expect(needs.message.contains("RAW helper"))
        let other = ViewerProblem.cannotDevelop("unsupported camera NIKON Z 9: High Efficiency")
        #expect(!other.needsRawlerHelper)
        #expect(other.actions.isEmpty)
        #expect(!ViewerProblem.renderFailed("needs the rawler helper").needsRawlerHelper)
    }

    @Test func statusTexts() {
        #expect(RawlerHelperText.status(.notInstalled) == "Not installed")
        #expect(RawlerHelperText.status(.downloading(0.426)) == "Downloading… 43%")
        #expect(RawlerHelperText.status(.installed("0.8.0")) == "Installed (rawler 0.8.0)")
        #expect(RawlerHelperText.status(.installed("")) == "Installed")
        #expect(RawlerHelperText.size.hasSuffix("MB"))
        #expect(RawlerHelper.release.url.scheme == "https")
        #expect(RawlerHelper.release.sha256.count == 64)
    }
}
