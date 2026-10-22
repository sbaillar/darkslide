import CoreML
import DarkslideAI
import Foundation
import SwiftUI
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Fixes from the full review of the app outside App/, Model/, Viewer/: archive settings
/// that fail to apply, overlapping mirror syncs, and the shared model download helper.
@Suite("Archive settings are retried, not forgotten")
struct ArchiveApplyTests {
    struct Boom: Error {}

    static let base = ArchiveSettingsSnapshot(enabled: true, target: "/old", intervalMinutes: 10, amazonFolder: nil)

    @Test func aFailedConfigIsNotRecordedAsApplied() {
        var next = Self.base
        next.target = "/new"
        let r = RustArchiveService.applyAll(from: Self.base, to: next, setConfig: { _ in throw Boom() }, setAmazon: { _ in })
        #expect(r.applied == Self.base, "still the old target, so the next tick tries again")
        #expect(r.error?.hasPrefix("Archive settings not applied") == true)
        #expect(ArchiveDisplay.title(.failed(r.error ?? "")) == "Archive settings not applied")
        #expect(ArchiveDisplay.title(.failed("disk full")) == "Last sync failed")
    }

    @Test func eachPartIsRecordedOnItsOwn() {
        var next = Self.base
        next.target = "/new"
        next.amazonFolder = URL(fileURLWithPath: "/amazon")
        let r = RustArchiveService.applyAll(from: Self.base, to: next, setConfig: { _ in }, setAmazon: { _ in throw Boom() })
        #expect(r.applied.target == "/new" && r.applied.amazonFolder == nil)
        #expect(r.error?.contains("Amazon Photos folder") == true)
        let ok = RustArchiveService.applyAll(from: r.applied, to: next, setConfig: { _ in Issue.record("already applied") },
                                             setAmazon: { _ in })
        #expect(ok.applied == next && ok.error == nil)
    }

    @Test func attemptLogsAndReports() {
        #expect(RustArchiveService.attempt("ok") {})
        #expect(!RustArchiveService.attempt("bad") { throw Boom() })
    }
}

@Suite("Model downloads: one helper")
struct ModelFilesTests {
    func tmp() throws -> URL {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("ds-modelfiles-\(UUID())")
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    /// Streams more than it promised, then waits to be cancelled (a hostile host).
    struct Endless: ModelDownloader {
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            progress(10_000_000)
            while true { try await Task.sleep(for: .milliseconds(5)) }
        }
    }

    /// Writes a file bigger than the pinned size without reporting progress.
    struct Oversized: ModelDownloader {
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            try Data(repeating: 1, count: 2 << 20).write(to: destination)
        }
    }

    @Test func aDownloadPastItsPinnedSizeIsCutOff() async throws {
        let dir = try tmp()
        defer { try? FileManager.default.removeItem(at: dir) }
        let url = URL(string: "https://example.invalid/f.bin")!
        await #expect(throws: ModelFetchError.download("f.bin is larger than expected")) {
            try await ModelFiles.fetch(url, to: dir.appendingPathComponent("f"), sha256: "00", size: 100, path: "f.bin",
                                       using: Endless(), progress: { _ in })
        }
        await #expect(throws: ModelFetchError.download("f.bin is larger than expected")) {
            try await ModelFiles.fetch(url, to: dir.appendingPathComponent("g"), sha256: "00", size: 100, path: "f.bin",
                                       using: Oversized(), progress: { _ in })
        }
    }

    @Test func hashMismatchAndCustomCheck() async throws {
        let dir = try tmp()
        defer { try? FileManager.default.removeItem(at: dir) }
        struct Small: ModelDownloader {
            func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
                try Data("abc".utf8).write(to: destination)
            }
        }
        let url = URL(string: "https://example.invalid/a.bin")!
        let dst = dir.appendingPathComponent("a")
        await #expect(throws: ModelFetchError.checksum("a.bin")) {
            try await ModelFiles.fetch(url, to: dst, sha256: "00", size: 3, path: "a.bin", using: Small(), progress: { _ in })
        }
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        try await ModelFiles.fetch(url, to: dst, sha256: good, size: 3, path: "a.bin", using: Small(), progress: { _ in })
        try await ModelFiles.fetch(url, to: dst, sha256: "", size: 3, path: "a.bin", using: Small(), progress: { _ in }, check: { _ in true })
        await #expect(throws: ModelFetchError.checksum("a.bin")) {
            try await ModelFiles.fetch(url, to: dst, sha256: "", size: 3, path: "a.bin", using: Small(), progress: { _ in }, check: { _ in false })
        }
    }

    @Test func plainHttpIsRefused() async throws {
        let dir = try tmp()
        defer { try? FileManager.default.removeItem(at: dir) }
        await #expect(throws: SegmentationError.self) {
            try await URLSessionModelDownloader().download(URL(string: "http://example.invalid/f.bin")!,
                                                           to: dir.appendingPathComponent("f")) { _ in }
        }
        // Nor is a local file: local copies go through LocalModelDownloader / DirectoryModelDownloader.
        let src = dir.appendingPathComponent("src.bin")
        try Data("x".utf8).write(to: src)
        await #expect(throws: SegmentationError.self) {
            try await URLSessionModelDownloader().download(src, to: dir.appendingPathComponent("f")) { _ in }
        }
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent("f").path))
    }

    @Test func redirectsAreFollowedOnlyToHTTPS() {
        let task = URLSession.shared.dataTask(with: URL(string: "https://models.example/m.bin")!)
        let moved = HTTPURLResponse(url: URL(string: "https://models.example/m.bin")!, statusCode: 302, httpVersion: nil,
                                    headerFields: nil)!
        final class Seen: @unchecked Sendable { var url: URL? } // the handler is called synchronously
        func follows(_ target: String) -> Bool {
            let seen = Seen()
            HTTPSRedirectsOnly.shared.urlSession(URLSession.shared, task: task, willPerformHTTPRedirection: moved,
                                                 newRequest: URLRequest(url: URL(string: target)!)) { seen.url = $0?.url }
            return seen.url == URL(string: target)
        }
        #expect(follows("https://cdn.example/m.bin"))
        #expect(!follows("http://cdn.example/m.bin"))
        #expect(!follows("file:///etc/hosts"))
    }

    @Test func sweepTakesOnlyItsOwnPrefix() throws {
        let dir = try tmp()
        defer { try? FileManager.default.removeItem(at: dir) }
        for n in [".a-download-1", ".a-download-2", ".b-download-1", "keep"] {
            try FileManager.default.createDirectory(at: dir.appendingPathComponent(n), withIntermediateDirectories: true)
        }
        ModelFiles.sweepStaging(in: dir, prefix: ".a-download-")
        let left = try FileManager.default.contentsOfDirectory(atPath: dir.path).sorted()
        #expect(left == [".b-download-1", "keep"])
    }

    @MainActor
    @Test func skyIsADownloadableModelWithItsOwnStagingAndOldCopyRenamedIntoPlace() throws {
        let sky = DownloadableModelDescriptor.sky
        #expect(sky.stagingPrefix.hasPrefix(".DETR") && sky.compiledName.contains(DownloadableModelDescriptor.skyRevision.prefix(12)))
        let dir = try tmp()
        defer { try? FileManager.default.removeItem(at: dir) }
        let legacy = dir.appendingPathComponent(sky.name + ".mlmodelc", isDirectory: true)
        try FileManager.default.createDirectory(at: legacy, withIntermediateDirectories: true)
        try Data("compiled".utf8).write(to: legacy.appendingPathComponent("model.bin"))
        let other = dir.appendingPathComponent(".lama-download-x", isDirectory: true)
        try FileManager.default.createDirectory(at: other, withIntermediateDirectories: true)
        let s = SkySegmentation(root: dir)
        #expect(s.state == .ready, "the installed model is kept, not fetched again")
        #expect(!FileManager.default.fileExists(atPath: legacy.path))
        #expect(try Data(contentsOf: s.compiledURL.appendingPathComponent("model.bin")) == Data("compiled".utf8))
        #expect(FileManager.default.fileExists(atPath: other.path))
        // With both present the old one goes.
        try FileManager.default.createDirectory(at: legacy, withIntermediateDirectories: true)
        _ = SkySegmentation(root: dir)
        #expect(!FileManager.default.fileExists(atPath: legacy.path) && FileManager.default.fileExists(atPath: s.compiledURL.path))
    }

    @Test func orphanedSharedPrefixFoldersAreSweptOnceOld() throws {
        let dir = try tmp()
        defer { try? FileManager.default.removeItem(at: dir) }
        let old = dir.appendingPathComponent(".download-\(UUID().uuidString)")
        let fresh = dir.appendingPathComponent(".download-\(UUID().uuidString)")
        let named = dir.appendingPathComponent(".download-another-model") // not the old builds' shape
        for u in [old, fresh, named] { try FileManager.default.createDirectory(at: u, withIntermediateDirectories: true) }
        for u in [old, named] {
            try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSinceNow: -7200)], ofItemAtPath: u.path)
        }
        ModelFiles.sweepLegacyStaging(in: dir)
        #expect(!FileManager.default.fileExists(atPath: old.path) && FileManager.default.fileExists(atPath: fresh.path))
        #expect(FileManager.default.fileExists(atPath: named.path))
    }

    @MainActor
    @Test func aModelSweepsOnlyItsOwnStaleStagingBeforeItsFirstDownload() async throws {
        let dir = try tmp()
        defer { try? FileManager.default.removeItem(at: dir) }
        let files = ["a.bin": Data(repeating: 1, count: 64)]
        let d = Lay9SkyTests.descriptor(files)
        let mine = dir.appendingPathComponent(d.stagingPrefix + "crashed", isDirectory: true)
        let theirs = dir.appendingPathComponent(".other-download-x", isDirectory: true)
        for u in [mine, theirs] { try FileManager.default.createDirectory(at: u, withIntermediateDirectories: true) }
        let m = DownloadableModel(root: dir, descriptor: d, downloader: Lay9SkyTests.LocalDownloader(files: files), compile: { pkg in
            let out = pkg.deletingLastPathComponent().appendingPathComponent("out.mlmodelc")
            try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
            return out
        })
        _ = try await m.prepare()
        #expect(!FileManager.default.fileExists(atPath: mine.path))
        #expect(FileManager.default.fileExists(atPath: theirs.path))
    }
}

@Suite("Grid caches")
@MainActor
struct GridCacheTests {
    @Test func unchangedEditIsFingerprintedWithoutEncodingAgain() {
        let c = ThumbnailCache(provider: MockImageProvider(latency: .zero))
        c.tools = FakeDevelopTools()
        var shown = EditState()
        shown.exposure.ev = 1
        c.editedSource = { _ in shown }
        let p = samplePhoto()
        let first = c.fingerprint(p)
        for _ in 0..<20 { #expect(c.fingerprint(p) == first) }
        #expect(c.encodes == 1, "cell bodies re-ask every evaluation")
        shown.exposure.ev = 1.5
        let second = c.fingerprint(p)
        #expect(second != first && c.encodes == 2, "a changed edit is seen at once")
        shown.exposure.ev = 1
        #expect(c.fingerprint(p) == first, "same edit, same identity")
    }

    @Test func collapsedLayoutIsReusedUntilSomethingItDependsOnChanges() {
        func photo(_ id: Int64, t: Double, rating: Int = 0) -> Photo {
            var p = samplePhoto(id)
            p.meta.captureDate = Date(timeIntervalSince1970: 1_000 + t)
            p.rating = rating
            return p
        }
        var photos = [photo(1, t: 0), photo(2, t: 0.2, rating: 4), photo(3, t: 0.4), photo(4, t: 10)]
        let s = StackState()
        s.update(photos: photos)
        s.collapseAll()
        let ids = photos.map(\.id)
        let lookup: (PhotoID) -> Photo? = { id in photos.first { $0.id == id } }
        #expect(s.displayIDs(from: ids, lookup: lookup) == [2, 4].map(PhotoID.init))
        #expect(s.displayBuilds == 1)
        for _ in 0..<10 { _ = s.displayIDs(from: ids, lookup: lookup) }
        #expect(s.displayBuilds == 1, "a selection or focus change re-evaluates the grid, not the layout")
        photos[2].rating = 5 // a rating promotes another frame as the stack's face
        #expect(s.displayIDs(from: ids, lookup: lookup) == [3, 4].map(PhotoID.init))
        #expect(s.displayBuilds == 2)
        #expect(s.displayIDs(from: ids.filter { $0 != PhotoID(4) }, lookup: lookup) == [3].map(PhotoID.init))
        #expect(s.displayBuilds == 3, "a filter change rebuilds")
        s.expandAll()
        #expect(s.displayIDs(from: ids, lookup: lookup) == ids)
    }
}

@Suite("Accessibility announcements")
@MainActor
struct AnnouncementTests {
    @Test func toastsAndFailedSavesAreAnnounced() {
        var heard: [(String, Announcer.Priority)] = []
        let saved = Announcer.sink
        Announcer.sink = { heard.append(($0, $1)) }
        defer { Announcer.sink = saved }
        Announcer.announce(Toast("Rated 3"))
        Announcer.announce(Toast("Export failed", isError: true))
        Announcer.announce("")
        Announcer.announce(saveState: .saved, from: .pending(1))
        Announcer.announce(saveState: .saving, from: .saved)
        Announcer.announce(saveState: .pending(2), from: .saved)
        Announcer.announce(saveState: .failed("disk full", pending: 3), from: .saving)
        Announcer.announce(saveState: .failed("disk full", pending: 4), from: .failed("disk full", pending: 3))
        Announcer.announce(saveState: .dropped("session closed", count: 1), from: .failed("disk full", pending: 4))
        #expect(heard.map(\.0) == ["Rated 3", "Export failed", "3 unsaved changes. Saving failed: disk full",
                                   "1 change not saved. session closed"])
        #expect(heard.map(\.1) == [.normal, .urgent, .urgent, .urgent])
    }

    @Test func viewerAndRatingLabels() {
        var p = samplePhoto(7)
        p.rating = 4
        let label = PhotoAccessibility.viewerLabel(p)
        #expect(label.contains(p.filename) && label.contains("4 stars") && !label.contains("image"))
        #expect(PhotoAccessibility.adjustedRating(5, increment: true) == 5)
        #expect(PhotoAccessibility.adjustedRating(0, increment: false) == 0)
        #expect(PhotoAccessibility.adjustedRating(2, increment: true) == 3)
        #expect(PhotoAccessibility.adjustedRating(2, increment: false) == 1)
    }

    @Test func reduceMotionDropsAnimationAndSlides() {
        #expect(Motion.animation(.snappy, reduce: true) == nil)
        #expect(Motion.animation(.snappy, reduce: false) != nil)
        _ = Motion.transition(.move(edge: .bottom), reduce: true)
        _ = Motion.transition(.move(edge: .bottom), reduce: false)
    }
}

@Suite("Jev key rejection is surfaced")
@MainActor
struct JevAuthTests {
    struct Failing: NaturalLanguageEditService {
        let error: Error
        func intent(for text: String) async throws -> EditIntent { throw error }
    }

    final class Seen: @unchecked Sendable {
        private let lock = NSLock()
        private var errors: [Error] = []
        func add(_ e: Error) { lock.withLock { errors.append(e) } }
        var count: Int { lock.withLock { errors.count } }
    }

    @Test func aRevokedKeyIsReportedAndTheNextBackendStillRuns() async throws {
        let seen = Seen()
        let guarded = GuardedEditService(models: [Failing(error: JevEditService.JevError.http(401))], onModelError: { seen.add($0) })
        _ = try await guarded.intent(for: "make it blorptastic")
        #expect(seen.count == 1)
        #expect(JevSettings.isAuthFailure(JevEditService.JevError.http(401)) && JevSettings.isAuthFailure(JevEditService.JevError.http(403)))
        #expect(!JevSettings.isAuthFailure(JevEditService.JevError.http(500)) && !JevSettings.isAuthFailure(URLError(.timedOut)))
    }

    @Test func settingsShowARejectedKeyUntilANewOneIsSaved() {
        let defaults = UserDefaults(suiteName: "jev-auth-\(UUID())")!
        let stored = Nl9Locked<String?>("old")
        let store = JevSettings.KeyStore(read: { stored.withLock { $0 } }, write: { k in stored.withLock { $0 = k }; return true })
        let jev = JevSettings(defaults: defaults, store: store)
        let service = makeEditService(jev: jev, foundationModels: false)
        service.onModelError(JevEditService.JevError.http(401))
        #expect(jev.keyRejected)
        let m = AISettingsModel(jev: jev, foundationModelsAvailable: false)
        #expect(m.message?.contains("rejected") == true)
        m.keyEntry = "new"
        m.saveKey()
        #expect(!jev.keyRejected)
        service.onModelError(URLError(.notConnectedToInternet))
        #expect(!jev.keyRejected, "an offline Mac isn't a bad key")
    }
}
