import CryptoKit
import Foundation
import Testing
@testable import DarkslideKit

private func sha(_ url: URL) throws -> String {
    SHA256.hash(data: try Data(contentsOf: url)).map { String(format: "%02x", $0) }.joined()
}

/// Smart folders through the real FFI + ds-session on copies of the sample RAWs.
@Suite(.serialized) struct SmartFolderLiveTests {
    func recents() -> RecentFolders {
        RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!)
    }

    @MainActor
    func model(_ store: RustSessionStore) -> AppModel {
        let settings = makeSettings()
        settings.onboardingComplete = true
        settings.autoAdvance = false
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        return m
    }

    /// Counts keyed by built-in key (or name for the user's folders).
    @MainActor
    func byKey(_ m: AppModel) -> [String: Int] {
        Dictionary(uniqueKeysWithValues: m.smartFolders.folders.map { ($0.builtin ?? $0.name, m.smartFolders.counts[$0.id] ?? -1) })
    }

    @Test func countsAreConsistentAcrossRelaunchAndBuiltinsComeBack() async throws {
        guard let dir = try sampleFolderCopy(limit: 8) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = recents()
        let s1 = RustSessionStore(recents: r)
        let info = try await s1.add(folder: dir)
        let m1 = await model(s1)
        await m1.open(info)
        let ids = await m1.visibleIDs
        #expect(ids.count == 8)
        await MainActor.run {
            for (i, rating) in [5, 5, 3, 1].enumerated() {
                m1.focus(ids[i], extend: false)
                m1.setRating(rating)
            }
            m1.focus(ids[4], extend: false)
            m1.setTag(.red)
            m1.focus(ids[5], extend: false)
            m1.setPick(.picked)
            m1.focus(ids[6], extend: false)
            m1.setPick(.rejected)
        }
        let user = await m1.smartFolders.create(name: "Three up", criteria: SmartCriteria(minRating: 3),
                                                photos: m1.photos)
        #expect(user != nil)
        await m1.saver.flush()
        let before = await byKey(m1)
        #expect(before["rating:5"] == 2 && before["rating:4"] == 0 && before["rating:3"] == 1 && before["rating:1"] == 1)
        #expect(before["unrated"] == 4 && before["picked"] == 1 && before["rejected"] == 1)
        #expect(before["tag:red"] == 1 && before["Three up"] == 3)

        // Relaunch: a fresh store and model see the same folders and counts.
        let s2 = RustSessionStore(recents: r)
        let listed = try await s2.listSessions()
        let m2 = await model(s2)
        await m2.open(listed[0])
        #expect(await byKey(m2) == before)
        let (counts, folders, photos) = await MainActor.run { (m2.smartFolders.counts, m2.smartFolders.folders, m2.photos) }
        #expect(await counts == bruteCounts(m2.smartFolders, photos))
        #expect(folders.allSatisfy { $0.id > 0 }, "every folder is persisted in the session now (tag:red too)")
        // Swift's local evaluation agrees with ds-session's SQL for every folder.
        for f in folders {
            let rust = try await s2.storeQuery(f.criteria, in: listed[0])
            #expect(rust.count == counts[f.id], "\(f.name)")
        }
        // ⌘5 shows exactly the two 5-star photos.
        await MainActor.run {
            CommandCatalog.perform(.showRating5, model: m2)
            #expect(Set(m2.visibleIDs) == Set(ids.prefix(2)))
        }

        // A built-in removed from albums.json comes back on the next open.
        let albums = dir.appendingPathComponent(".ds/session/.ds/settings/albums.json")
        var json = try #require(try JSONSerialization.jsonObject(with: Data(contentsOf: albums)) as? [String: Any])
        let list = try #require(json["albums"] as? [[String: Any]])
        json["albums"] = list.filter { ($0["builtin"] as? String) != "picked" }
        try JSONSerialization.data(withJSONObject: json).write(to: albums)
        let s3 = RustSessionStore(recents: r)
        let m3 = await model(s3)
        await m3.open(try await s3.listSessions()[0])
        let again = await byKey(m3)
        #expect(again["picked"] == 1 && again == before)
    }

    @Test func mirrorToFinderMakesOnlySymlinksAndNeverTouchesOriginals() async throws {
        guard let dir = try sampleFolderCopy(limit: 4) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let fm = FileManager.default
        let originals = try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil).filter { $0.pathExtension == "ARW" }
        var hashes: [URL: String] = [:]
        for u in originals { hashes[u] = try sha(u) }
        let r = recents()
        let s1 = RustSessionStore(recents: r)
        let info = try await s1.add(folder: dir)
        let m = await model(s1)
        await m.open(info)
        let ids = await m.visibleIDs
        await MainActor.run {
            m.smartFolders.mirrorDelay = .milliseconds(10)
            m.focus(ids[0], extend: false)
            m.setRating(5)
        }
        let root = try #require(await s1.storeMirrorRoot(for: info))
        #expect(root.lastPathComponent == "Smart Folders")
        #expect(root.deletingLastPathComponent().resolvingSymlinksInPath().path == dir.resolvingSymlinksInPath().path,
                "next to the originals")
        let five = try #require(await m.smartFolders.ratingFolder(5))
        await m.smartFolders.setMirror(five, on: true, photos: m.photos)
        await m.smartFolders.waitForMirror()
        let mirrorDir = root.appendingPathComponent("5 stars")
        func entries() throws -> [String] { try fm.contentsOfDirectory(atPath: mirrorDir.path).sorted() }
        #expect(try entries().count == 1)
        // Another 5★ updates the mirror (debounced).
        await MainActor.run {
            m.focus(ids[1], extend: false)
            m.setRating(5)
        }
        await m.smartFolders.waitForMirror()
        #expect(try entries().count == 2)
        for e in try entries() {
            let p = mirrorDir.appendingPathComponent(e).path
            #expect(try fm.attributesOfItem(atPath: p)[.type] as? FileAttributeType == .typeSymbolicLink)
            let target = URL(fileURLWithPath: try fm.destinationOfSymbolicLink(atPath: p)).resolvingSymlinksInPath().path
            #expect(hashes.keys.map { $0.resolvingSymlinksInPath().path }.contains(target), "links point at an original")
        }
        await m.saver.flush()
        // Reopening doesn't pick the links up as new photos.
        let s2 = RustSessionStore(recents: r)
        let reopened = try await s2.photos(in: try await s2.listSessions()[0])
        #expect(reopened.count == originals.count)
        // Turning it off removes the links, not the folder or the originals.
        await m.smartFolders.setMirror(await m.smartFolders.ratingFolder(5)!, on: false, photos: m.photos)
        #expect(try entries().isEmpty)
        for (u, h) in hashes { #expect(try sha(u) == h, "original \(u.lastPathComponent) unchanged") }
    }
}
