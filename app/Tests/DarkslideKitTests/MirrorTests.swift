import CryptoKit
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// In-memory file system: `dirs` directories, `files` regular files, `links` symlinks (path → target).
final class FakeMirrorFS: MirrorFileSystem, @unchecked Sendable {
    private let lock = NSLock()
    private var dirs: Set<String> = []
    private var files: [String: Data] = [:]
    private var links: [String: String] = [:]
    private(set) var removed: [String] = []

    func addFile(_ url: URL) { lock.withLock { files[url.path] = Data() } }
    func link(_ url: URL) -> String? { lock.withLock { links[url.path] } }
    /// Links outside the mirror's manifest (the manifest is a file).
    var linkCount: Int { lock.withLock { links.count } }
    func hasFile(_ url: URL) -> Bool { lock.withLock { files[url.path] != nil } }

    func kind(_ url: URL) -> MirrorEntryKind {
        lock.withLock {
            if links[url.path] != nil { return .symlink }
            if dirs.contains(url.path) { return .directory }
            return files[url.path] != nil ? .other : .missing
        }
    }
    func makeDirectory(_ url: URL) throws {
        try lock.withLock {
            if links[url.path] != nil || files[url.path] != nil { throw MirrorError.io(url.path, EEXIST) }
            dirs.insert(url.path)
        }
    }
    func contents(of dir: URL) throws -> [URL] {
        lock.withLock {
            (Array(files.keys) + Array(links.keys) + Array(dirs)).filter { ($0 as NSString).deletingLastPathComponent == dir.path }
                .sorted().map { URL(fileURLWithPath: $0) }
        }
    }
    func symlinkDestination(_ url: URL) throws -> String {
        try lock.withLock { guard let t = links[url.path] else { throw MirrorError.io(url.path, 2) }; return t }
    }
    func createSymlink(at url: URL, to destination: URL) throws { lock.withLock { links[url.path] = destination.path } }
    func removeLink(_ url: URL) throws {
        lock.withLock {
            links[url.path] = nil
            removed.append(url.path)
        }
    }
    func canonicalDirectory(_ url: URL) -> URL { url.standardizedFileURL }
    func read(_ url: URL) -> Data? { lock.withLock { files[url.path] } }
    func write(_ data: Data, to url: URL) throws {
        lock.withLock {
            links[url.path] = nil
            files[url.path] = data
        }
    }
}

private func photo(_ id: Int64, _ path: String, rating: Int = 5) -> Photo {
    var p = samplePhoto(id)
    p.url = URL(fileURLWithPath: path)
    p.rating = rating
    return p
}

private func sha(_ url: URL) throws -> String {
    SHA256.hash(data: try Data(contentsOf: url)).map { String(format: "%02x", $0) }.joined()
}

@Suite struct MirrorTests {
    let root = URL(fileURLWithPath: "/Volumes/Card/Shoot/Smart Folders")

    @Test func syncCreatesAndPrunesOnlySymlinks() throws {
        let fs = FakeMirrorFS()
        let mirror = SmartFolderMirror(root: root, fs: fs)
        let dir = mirror.directory(for: "5 stars")
        let a = photo(1, "/Volumes/Card/Shoot/A.ARW"), b = photo(2, "/Volumes/Card/Shoot/B.ARW")
        // Same file name from another folder: gets a distinct link name.
        let b2 = photo(3, "/Volumes/Card/Shoot/sub/B.ARW")
        var r = try mirror.sync(name: "5 stars", members: [a, b, b2, samplePhoto(4)])
        #expect(r.created == 3 && r.removed == 0, "photos without an original are skipped")
        #expect(fs.link(dir.appendingPathComponent("A.ARW")) == a.url!.path)
        #expect(fs.link(dir.appendingPathComponent("B (3).ARW")) == b2.url!.path)
        // A user's own file in the mirror folder is never touched, and blocks that link name.
        let own = dir.appendingPathComponent("notes.txt")
        fs.addFile(own)
        let clash = dir.appendingPathComponent("C.ARW")
        fs.addFile(clash)
        r = try mirror.sync(name: "5 stars", members: [a, photo(5, "/x/C.ARW")])
        #expect(r.removed == 2 && r.created == 0)
        #expect(fs.hasFile(own) && fs.hasFile(clash) && fs.linkCount == 1)
        #expect(try mirror.sync(name: "5 stars", members: [a]) == (0, 0), "idempotent")
        // A link retargeted elsewhere isn't the mirror's any more: it stays, untouched (M4).
        try fs.createSymlink(at: dir.appendingPathComponent("A.ARW"), to: URL(fileURLWithPath: "/elsewhere"))
        #expect(try mirror.sync(name: "5 stars", members: [a]) == (0, 0))
        #expect(try mirror.clear(id: 0) == 0)
        #expect(fs.hasFile(own) && fs.link(dir.appendingPathComponent("A.ARW")) == "/elsewhere")
        #expect(fs.removed.allSatisfy { ($0 as NSString).deletingLastPathComponent == dir.path })
    }

    @Test func refusesToDeleteAnythingButItsOwnSymlinks() throws {
        let fs = FakeMirrorFS()
        let mirror = SmartFolderMirror(root: root, fs: fs)
        let dir = mirror.directory(for: "Picked")
        let file = dir.appendingPathComponent("real.ARW")
        fs.addFile(file)
        #expect(throws: MirrorError.refused(file.path)) { try mirror.removeLink(file, in: dir) }
        // A symlink outside the mirror folder (e.g. next to the originals).
        let outside = URL(fileURLWithPath: "/Volumes/Card/Shoot/link.ARW")
        try fs.createSymlink(at: outside, to: URL(fileURLWithPath: "/x"))
        #expect(throws: MirrorError.refused(outside.path)) { try mirror.removeLink(outside, in: dir) }
        // A "mirror folder" that isn't directly inside the root.
        let nested = dir.appendingPathComponent("deeper", isDirectory: true)
        let deepLink = nested.appendingPathComponent("l")
        try fs.createSymlink(at: deepLink, to: URL(fileURLWithPath: "/x"))
        #expect(throws: MirrorError.refused(deepLink.path)) { try mirror.removeLink(deepLink, in: nested) }
        #expect(fs.removed.isEmpty && fs.hasFile(file))
    }

    @Test func folderNamesAreFinderSafe() {
        #expect(SmartFolderMirror.directoryName("5 stars") == "5 stars")
        #expect(SmartFolderMirror.directoryName("a/b:c") == "a-b-c")
        #expect(SmartFolderMirror.directoryName("..hidden") == "hidden")
        #expect(SmartFolderMirror.directoryName(" / ") == "-")
        #expect(SmartFolderMirror.directoryName("  ") == "Untitled")
        let m = SmartFolderMirror(root: root)
        #expect(m.directory(for: "../../etc").deletingLastPathComponent().path == root.path)
    }

    /// Real files in a temp dir: originals are byte-identical afterwards and the
    /// mirror holds only symlinks.
    @Test func realFileSystemMirrorLeavesOriginalsUntouched() throws {
        let fm = FileManager.default
        let base = fm.temporaryDirectory.appendingPathComponent("smart4-mirror-\(UUID())")
        defer { try? fm.removeItem(at: base) }
        try fm.createDirectory(at: base, withIntermediateDirectories: true)
        var photos: [Photo] = []
        var hashes: [URL: String] = [:]
        for i in 1...4 {
            let u = base.appendingPathComponent("DSC0\(i).ARW")
            try Data("raw bytes \(i)".utf8).write(to: u)
            hashes[u] = try sha(u)
            photos.append(photo(Int64(i), u.path))
        }
        let mirror = SmartFolderMirror(root: base.appendingPathComponent("Smart Folders"))
        #expect(try mirror.sync(name: "5 stars", members: photos) == (4, 0))
        #expect(try mirror.sync(name: "5 stars", members: Array(photos.prefix(2))) == (0, 2))
        let dir = mirror.directory(for: "5 stars")
        let entries = try fm.contentsOfDirectory(atPath: dir.path).sorted()
        #expect(entries == ["DSC01.ARW", "DSC02.ARW"])
        for e in entries {
            let attrs = try fm.attributesOfItem(atPath: dir.appendingPathComponent(e).path)
            #expect(attrs[.type] as? FileAttributeType == .typeSymbolicLink)
            #expect(try fm.destinationOfSymbolicLink(atPath: dir.appendingPathComponent(e).path) == base.appendingPathComponent(e).path)
        }
        // Not a hard link either: originals still have one link each.
        for (u, h) in hashes {
            #expect(try sha(u) == h)
            #expect((try fm.attributesOfItem(atPath: u.path)[.referenceCount] as? Int) == 1)
        }
        // Real removeLink refuses a regular file even if called directly.
        let fs = RealMirrorFileSystem()
        let regular = dir.appendingPathComponent("user.txt")
        try Data("mine".utf8).write(to: regular)
        #expect(throws: MirrorError.refused(regular.path)) { try mirror.removeLink(regular, in: dir) }
        #expect(throws: MirrorError.self) { try fs.removeLink(dir) }
        #expect(try mirror.clear(id: 0) == 2)
        #expect(try fm.contentsOfDirectory(atPath: dir.path) == ["user.txt"])
        for (u, h) in hashes { #expect(try sha(u) == h) }
        #expect(try fs.contents(of: base.appendingPathComponent("nope")).isEmpty)
    }

    @MainActor
    @Test func controllerMirrorsDebouncedAndClearsWhenTurnedOff() async throws {
        let m = makeModel(counts: [30])
        await m.start()
        let sf = m.smartFolders
        let fs = FakeMirrorFS()
        sf.mirrorFS = fs
        sf.mirrorDelay = .milliseconds(10)
        let five = sf.ratingFolder(5)!
        // The demo store has no mirror root: turning it on persists the flag only.
        await sf.setMirror(five, on: true, photos: m.photos)
        #expect(sf.ratingFolder(5)!.mirror)
        await sf.waitForMirror()
        #expect(fs.linkCount == 0)
        await sf.syncMirrors(root: root)
        #expect(fs.linkCount == 0, "demo photos have no originals to link")
        await sf.setMirror(sf.ratingFolder(5)!, on: false, photos: m.photos)
        #expect(!sf.ratingFolder(5)!.mirror)
    }

    /// Two overlapping syncs (a slow volume and a second debounce) must not interleave on the
    /// manifest: the later save would drop links the earlier one created.
    @MainActor
    @Test func overlappingSyncsRunOneAfterTheOther() async throws {
        final class ProbeFS: MirrorFileSystem, @unchecked Sendable {
            let inner = FakeMirrorFS()
            private let lock = NSLock()
            private var active = 0
            private(set) var maxActive = 0
            private func probe<T>(_ body: () throws -> T) rethrows -> T {
                lock.withLock {
                    active += 1
                    maxActive = max(maxActive, active)
                }
                defer { lock.withLock { active -= 1 } }
                Thread.sleep(forTimeInterval: 0.03)
                return try body()
            }
            func kind(_ url: URL) -> MirrorEntryKind { probe { inner.kind(url) } }
            func makeDirectory(_ url: URL) throws { try probe { try inner.makeDirectory(url) } }
            func contents(of dir: URL) throws -> [URL] { try probe { try inner.contents(of: dir) } }
            func symlinkDestination(_ url: URL) throws -> String { try probe { try inner.symlinkDestination(url) } }
            func createSymlink(at url: URL, to destination: URL) throws { try probe { try inner.createSymlink(at: url, to: destination) } }
            func removeLink(_ url: URL) throws { try probe { try inner.removeLink(url) } }
            func canonicalDirectory(_ url: URL) -> URL { inner.canonicalDirectory(url) }
            func read(_ url: URL) -> Data? { probe { inner.read(url) } }
            func write(_ data: Data, to url: URL) throws { try probe { try inner.write(data, to: url) } }
        }
        let m = makeModel(counts: [30])
        await m.start()
        let sf = m.smartFolders
        let fs = ProbeFS()
        sf.mirrorFS = fs
        await sf.setMirror(sf.ratingFolder(5)!, on: true, photos: m.photos)
        async let a: Void = sf.syncMirrors(root: root)
        async let b: Void = sf.syncMirrors(root: root)
        _ = await (a, b)
        #expect(fs.maxActive == 1, "syncs overlapped: \(fs.maxActive) file-system calls at once")
    }

    @MainActor
    @Test func aCleanSyncKeepsAnUnrelatedError() async throws {
        let m = makeModel(counts: [30])
        await m.start()
        let sf = m.smartFolders
        sf.mirrorFS = FakeMirrorFS()
        sf.lastError = "Couldn't rename the folder"
        await sf.syncMirrors(root: root)
        #expect(sf.lastError == "Couldn't rename the folder")
        sf.lastError = "Mirror to Finder: disk full"
        await sf.syncMirrors(root: root)
        #expect(sf.lastError == nil, "the mirror's own earlier error clears once it works")
    }
}
