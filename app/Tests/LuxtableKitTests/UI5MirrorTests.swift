import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Mirror to Finder on the real file system (R4 review M4): symlinked folders,
/// case-only clashes, shared names, and only-our-own-links.
@Suite struct UI5MirrorTests {
    let fm = FileManager.default

    /// A temp dir with originals `names` and an (empty) mirror root path.
    func sandbox(_ names: [String]) throws -> (base: URL, originals: [URL], root: URL) {
        let base = URL(fileURLWithPath: RealMirrorFileSystem().canonicalDirectory(fm.temporaryDirectory).path)
            .appendingPathComponent("ui5-mirror-\(UUID())", isDirectory: true)
        try fm.createDirectory(at: base, withIntermediateDirectories: true)
        var urls: [URL] = []
        for n in names {
            let u = base.appendingPathComponent(n)
            try fm.createDirectory(at: u.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data("raw \(n)".utf8).write(to: u)
            urls.append(u)
        }
        return (base, urls, base.appendingPathComponent("Smart Folders", isDirectory: true))
    }

    func photo(_ id: Int64, _ url: URL) -> Photo {
        var p = samplePhoto(id)
        p.url = url
        return p
    }

    /// Name → (is symlink, destination) of a directory's entries.
    func listing(_ dir: URL) throws -> [String: String] {
        var out: [String: String] = [:]
        for n in try fm.contentsOfDirectory(atPath: dir.path) {
            let p = dir.appendingPathComponent(n).path
            out[n] = (try? fm.destinationOfSymbolicLink(atPath: p)).map { "-> \($0)" } ?? "file"
        }
        return out
    }

    @Test func symlinkedMirrorFolderIsRefusedAndTheUsersFolderIsUntouched() throws {
        let (base, originals, root) = try sandbox(["A.ARW"])
        defer { try? fm.removeItem(at: base) }
        // The user's own folder, holding a file and their own symlink.
        let real = base.appendingPathComponent("MyRealFolder", isDirectory: true)
        try fm.createDirectory(at: real, withIntermediateDirectories: true)
        try Data("mine".utf8).write(to: real.appendingPathComponent("own.txt"))
        try fm.createSymbolicLink(atPath: real.appendingPathComponent("my-link.ARW").path, withDestinationPath: "/x")
        let before = try listing(real)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        try fm.createSymbolicLink(at: root.appendingPathComponent("5 stars"), withDestinationURL: real)

        let r = try SmartFolderMirror(root: root).sync([
            MirrorFolder(id: 1, name: "5 stars", members: [photo(1, originals[0])]),
            MirrorFolder(id: 2, name: "Picked", members: [photo(1, originals[0])]),
        ])
        #expect(r.errors.count == 1 && r.errors[0].contains("5 stars"), "\(r.errors)")
        #expect(try listing(real) == before, "nothing written or deleted through the symlink")
        // The other folder still ran (a per-folder error doesn't abort the rest).
        #expect(r.created == 1)
        #expect(try listing(root.appendingPathComponent("Picked")) == ["A.ARW": "-> \(originals[0].path)"])
    }

    @Test func symlinkedRootIsRefused() throws {
        let (base, originals, root) = try sandbox(["A.ARW"])
        defer { try? fm.removeItem(at: base) }
        let real = base.appendingPathComponent("Elsewhere", isDirectory: true)
        try fm.createDirectory(at: real, withIntermediateDirectories: true)
        try fm.createSymbolicLink(at: root, withDestinationURL: real)
        let mirror = SmartFolderMirror(root: root)
        #expect(throws: MirrorError.self) {
            try mirror.sync([MirrorFolder(id: 1, name: "5 stars", members: [photo(1, originals[0])])])
        }
        #expect(try mirror.clear(id: 1) == 0)
        #expect(try fm.contentsOfDirectory(atPath: real.path).isEmpty)
    }

    @Test func caseOnlyClashIsDisambiguatedNotAborted() throws {
        let (base, originals, root) = try sandbox(["x/a.arw", "y/A.ARW", "z/B.ARW"])
        defer { try? fm.removeItem(at: base) }
        let mirror = SmartFolderMirror(root: root)
        let members = [photo(7, originals[0]), photo(8, originals[1]), photo(9, originals[2])]
        let r = try mirror.sync([MirrorFolder(id: 1, name: "5 stars", members: members)])
        #expect(r.errors.isEmpty && r.created == 3)
        let dir = root.appendingPathComponent("5 stars")
        #expect(try listing(dir) == ["a.arw": "-> \(originals[0].path)", "A (8).ARW": "-> \(originals[1].path)",
                                     "B.ARW": "-> \(originals[2].path)"])
        #expect(try mirror.sync([MirrorFolder(id: 1, name: "5 stars", members: members)]) == MirrorReport(),
                "idempotent")
    }

    @Test func foldersWithTheSameFinderNameGetTheirOwnDirectories() throws {
        let (base, originals, root) = try sandbox(["A.ARW", "B.ARW"])
        defer { try? fm.removeItem(at: base) }
        let mirror = SmartFolderMirror(root: root)
        let folders = [
            MirrorFolder(id: 0x1001, name: "Best/Keep", members: [photo(1, originals[0])]),
            MirrorFolder(id: 0x2002, name: "Best:Keep", members: [photo(2, originals[1])]),
            MirrorFolder(id: 0x3003, name: "best-keep", members: [photo(1, originals[0])]),
        ]
        #expect(try mirror.sync(folders).created == 3)
        #expect(Set(try fm.contentsOfDirectory(atPath: root.path)) ==
                ["Best-Keep", "Best-Keep (2002)", "best-keep (3003)", SmartFolderMirror.manifestName])
        // No thrash: each sync keeps the others' links.
        #expect(try mirror.sync(folders) == MirrorReport())
        #expect(try listing(root.appendingPathComponent("Best-Keep (2002)")) == ["B.ARW": "-> \(originals[1].path)"])
    }

    /// A mirror made before the rename lists its links in the old manifest: they are
    /// still ours (removed when they leave the folder), and the next save uses the new name.
    @Test func linksListedInTheOldManifestAreStillOurs() throws {
        let (base, originals, root) = try sandbox(["A.ARW", "B.ARW"])
        defer { try? fm.removeItem(at: base) }
        let mirror = SmartFolderMirror(root: root)
        try mirror.sync([MirrorFolder(id: 1, name: "Picks", members: [photo(1, originals[0]), photo(2, originals[1])])])
        try fm.moveItem(at: root.appendingPathComponent(SmartFolderMirror.manifestName),
                        to: root.appendingPathComponent(SmartFolderMirror.legacyManifestName))
        let r = try mirror.sync([MirrorFolder(id: 1, name: "Picks", members: [photo(1, originals[0])])])
        #expect(r.removed == 1)
        #expect(try listing(root.appendingPathComponent("Picks")) == ["A.ARW": "-> \(originals[0].path)"])
        #expect(fm.fileExists(atPath: root.appendingPathComponent(SmartFolderMirror.manifestName).path))
    }

    @Test func onlyLinksTheMirrorCreatedAreEverRemoved() throws {
        let (base, originals, root) = try sandbox(["A.ARW", "B.ARW"])
        defer { try? fm.removeItem(at: base) }
        let mirror = SmartFolderMirror(root: root)
        try mirror.sync([MirrorFolder(id: 1, name: "Picks", members: [photo(1, originals[0]), photo(2, originals[1])])])
        let dir = root.appendingPathComponent("Picks")
        // The user adds their own link, and repoints one of ours.
        try fm.createSymbolicLink(atPath: dir.appendingPathComponent("mine.ARW").path, withDestinationPath: originals[0].path)
        try fm.removeItem(at: dir.appendingPathComponent("B.ARW"))
        try fm.createSymbolicLink(atPath: dir.appendingPathComponent("B.ARW").path, withDestinationPath: "/somewhere/else")
        let r = try mirror.sync([MirrorFolder(id: 1, name: "Picks", members: [])])
        #expect(r.removed == 1)
        #expect(try listing(dir) == ["mine.ARW": "-> \(originals[0].path)", "B.ARW": "-> /somewhere/else"])
        #expect(try mirror.clear(id: 1) == 0)
        #expect(try listing(dir).count == 2)
    }

    @Test func renamedOrDroppedFoldersLoseOnlyTheirOwnLinks() throws {
        let (base, originals, root) = try sandbox(["A.ARW"])
        defer { try? fm.removeItem(at: base) }
        let mirror = SmartFolderMirror(root: root)
        let a = photo(1, originals[0])
        try mirror.sync([MirrorFolder(id: 1, name: "Old", members: [a]), MirrorFolder(id: 2, name: "Other", members: [a])])
        try Data("note".utf8).write(to: root.appendingPathComponent("Old/notes.txt"))
        // Renamed: links move to the new folder; the user's file stays in the old one.
        var r = try mirror.sync([MirrorFolder(id: 1, name: "New", members: [a]), MirrorFolder(id: 2, name: "Other", members: [a])])
        #expect(r.created == 1 && r.removed == 1)
        #expect(try listing(root.appendingPathComponent("Old")) == ["notes.txt": "file"])
        #expect(try listing(root.appendingPathComponent("New")).keys.sorted() == ["A.ARW"])
        // "Other" turned off: its links go (clearOthers), "New" stays.
        r = try mirror.sync([MirrorFolder(id: 1, name: "New", members: [a])])
        #expect(r.removed == 1 && r.created == 0)
        #expect(try listing(root.appendingPathComponent("Other")).isEmpty)
    }

    @Test func manifestSymlinkIsReplacedNotWrittenThrough() throws {
        let (base, originals, root) = try sandbox(["A.ARW"])
        defer { try? fm.removeItem(at: base) }
        let victim = base.appendingPathComponent("victim.txt")
        try Data("keep me".utf8).write(to: victim)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        try fm.createSymbolicLink(at: root.appendingPathComponent(SmartFolderMirror.manifestName), withDestinationURL: victim)
        try SmartFolderMirror(root: root).sync([MirrorFolder(id: 1, name: "5 stars", members: [photo(1, originals[0])])])
        #expect(try String(contentsOf: victim, encoding: .utf8) == "keep me")
        let fs = RealMirrorFileSystem()
        #expect(fs.kind(root.appendingPathComponent(SmartFolderMirror.manifestName)) == .other)
        #expect(fs.read(victim) != nil && fs.read(base) == nil)
        #expect(throws: MirrorError.self) { try fs.makeDirectory(victim) }
        #expect(throws: MirrorError.self) { try fs.createSymlink(at: victim, to: base) }
    }

    @Test func directoryNamesAreUniqueCaseInsensitively() {
        let n = SmartFolderMirror.directoryNames([(1, "Keep"), (0x22, "keep"), (0x33, "KEEP"), (3, "Other")])
        #expect(n[1] == "Keep" && n[3] == "Other")
        #expect(Set(n.values.map { $0.lowercased() }).count == n.count)
        #expect(SmartFolderMirror.describe(MirrorError.io("/p", ENOENT)).contains("No such file"))
        #expect(SmartFolderMirror.describe(MirrorError.refused("/p")).contains("refused"))
    }

    @MainActor
    @Test func controllerReportsPerFolderErrors() async throws {
        let (base, originals, root) = try sandbox(["A.ARW"])
        defer { try? fm.removeItem(at: base) }
        let m = makeModel(counts: [10])
        await m.start()
        let sf = m.smartFolders
        sf.mirrorDelay = .seconds(60)
        var withURL = m.photos
        for i in withURL.indices { withURL[i].url = originals[0] }
        sf.photosProvider = { withURL }
        await sf.setMirror(sf.ratingFolder(5)!, on: true, photos: m.photos)
        // A symlinked "5 stars" folder: the refresh reports it instead of writing through it.
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        try fm.createSymbolicLink(at: root.appendingPathComponent("5 stars"), withDestinationURL: base)
        await sf.syncMirrors(root: root)
        #expect(sf.lastError?.contains("5 stars") == true)
        try fm.removeItem(at: root.appendingPathComponent("5 stars"))
        await sf.syncMirrors(root: root)
        #expect(sf.lastError == nil)
    }
}
