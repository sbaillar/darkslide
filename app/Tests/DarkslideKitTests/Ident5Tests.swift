import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideKit

/// Round 5, identity on the live path: store keys per volume (R4-H1), content
/// identity and "file changed" restore (R4-H3), background refresh (R4-M1),
/// user-activity pauses (R4-M3), store-kept copies (R4-M5), Sessions counts (L12).
@Suite(.serialized) struct Ident5Tests {
    func recents() -> RecentFolders {
        RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!)
    }

    func tempBase() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("ds-store-\(UUID())")
    }

    /// Two network shares have no volume UUID: they used to share one key
    /// ("no-volume-id") for folders with the same sub-path.
    @Test func twoMountsWithTheSameSubPathGetDifferentStores() throws {
        let base = URL(fileURLWithPath: "/tmp/base")
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("ds-mounts-\(UUID())")
        let a = tmp.appendingPathComponent("pics/2026-08-06 Beach Trip")
        let b = tmp.appendingPathComponent("backup/2026-08-06 Beach Trip")
        // Same path on their volumes (pathOnVolume is the absolute path here);
        // the keys stand in for `smbfs //nas/pics` and `smbfs //nas/backup`.
        let key: (URL) -> String = { url in
            RustSessionStore.shortHash(url.path.contains("/pics/") ? "net:smbfs://nas/pics" : "net:smbfs://nas/backup")
        }
        let sa = RustSessionStore.fallbackStore(for: a, base: base, volumeKey: key)
        let sb = RustSessionStore.fallbackStore(for: b, base: base, volumeKey: key)
        #expect(sa != sb)
        #expect(sa.deletingLastPathComponent() != sb.deletingLastPathComponent(), "different volumes")
        #expect(RustSessionStore.folderIdentity(for: a, volumeKey: key) != RustSessionStore.folderIdentity(for: b, volumeKey: key))
        // A real local folder is keyed by its volume UUID.
        let local = FileManager.default.temporaryDirectory
        let uuid = try #require(try local.resourceValues(forKeys: [.volumeUUIDStringKey]).volumeUUIDString)
        #expect(RustSessionStore.volumeKey(for: local) == uuid)
        #expect(RustSessionStore.folderIdentity(for: local).hasPrefix(uuid + "|"))
        // Without a UUID: the mount source from Rust (statfs), hashed.
        let id = try volumeIdentity(folder: local.path)
        #expect(id.hasPrefix("dev:"))
        #expect(RustSessionStore.volumeKey(for: tmp.appendingPathComponent("gone")) == "vol-" + RustSessionStore.shortHash("unknown"))
    }

    /// The whole flow on real files: a sensor-data change is flagged in the
    /// background (the app lists again), the badge survives a relaunch, and
    /// "Restore Previous" brings the rating back; "Keep as New" clears it.
    @Test func fileChangedIsPersistentAndRestorable() async throws {
        let samples = sampleARWs()
        guard samples.count >= 3, let dir = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let rec = recents()
        let base = tempBase()
        defer { try? FileManager.default.removeItem(at: base) }
        let s = RustSessionStore(recents: rec, storeBase: base)
        let info = try await s.add(folder: dir)
        await s.waitForHashing(info.id)
        var photos = try await s.photos(in: info)
        #expect(photos.count == 2)
        for (i, p) in photos.enumerated() { var q = p; q.rating = 4 + i; try await s.save(photos: [q]) }
        // Replace the first file with a different photo (the third sample's bytes).
        let victim = try #require(photos.first?.url)
        try FileManager.default.removeItem(at: victim)
        try FileManager.default.copyItem(at: samples[2], to: victim)

        let updated = Locked<Int>(0)
        let s2 = RustSessionStore(recents: rec, storeBase: base)
        s2.noteUserActivity() // the user is active: background checks wait (R4-M3)
        let info2 = try await s2.add(folder: dir) { p in if p.phase == .updated { updated.withLock { $0 += 1 } } }
        photos = try await s2.photos(in: info2)
        #expect(photos.first { $0.url == victim }?.rating == 4, "provisional until checked")
        await s2.waitForHashing(info2.id)
        #expect(updated.withLock { $0 } == 1, "the app is told to list again")
        photos = try await s2.photos(in: info2)
        let changed = try #require(photos.first { $0.url == victim })
        #expect(changed.fileChanged && changed.rating == 0)
        // Relaunch: still flagged.
        let s3 = RustSessionStore(recents: rec, storeBase: base)
        let info3 = try await s3.add(folder: dir)
        photos = try await s3.photos(in: info3)
        let again = try #require(photos.first { $0.url == victim })
        #expect(again.fileChanged)
        #expect(try await s3.listSessions().first { $0.id == info3.id }?.photoCount == 2, "Sessions list count")
        try await s3.restorePrevious(again.id)
        photos = try await s3.photos(in: info3)
        let restored = try #require(photos.first { $0.url == victim })
        #expect(!restored.fileChanged && restored.rating == 4)
        // Replace again (with the other photo's bytes), then keep it as new.
        let other = try #require(photos.first { $0.url != victim }?.url)
        try FileManager.default.removeItem(at: victim)
        try FileManager.default.copyItem(at: other, to: victim)
        let s4 = RustSessionStore(recents: rec, storeBase: base)
        let info4 = try await s4.add(folder: dir)
        await s4.waitForHashing(info4.id)
        photos = try await s4.photos(in: info4)
        let fresh = try #require(photos.first { $0.url == victim })
        #expect(fresh.fileChanged)
        try await s4.dismissFileChanged(fresh.id)
        photos = try await s4.photos(in: info4)
        #expect(photos.first { $0.url == victim }?.fileChanged == false)
        await #expect(throws: (any Error).self) { try await s4.restoreToFolder(fresh.id) }
    }

    /// The app flow on the live store: the background pass flags a replaced
    /// file, the model lists again by itself, and Restore Previous / Keep as
    /// New act on it.
    @MainActor @Test func modelRestoresPreviousAndKeepsAsNewOnTheLivePath() async throws {
        let samples = sampleARWs()
        guard samples.count >= 3, let dir = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let base = tempBase()
        defer { try? FileManager.default.removeItem(at: base) }
        let store = RustSessionStore(recents: recents(), storeBase: base)
        let model = makeModel(store: store)
        await model.openFolder(dir)
        let first = try #require(model.photos.first)
        let victim = try #require(first.url)
        model.focus(first.id, extend: false)
        model.setRating(5)
        await model.saver.flush()
        try FileManager.default.removeItem(at: victim)
        try FileManager.default.copyItem(at: samples[2], to: victim)
        await model.openFolder(dir)
        // No manual reload: the `.updated` signal from the background pass lists again.
        #expect(await waitUntil(timeout: .seconds(20)) { model.photos.contains { $0.url == victim && $0.fileChanged } })
        let changed = try #require(model.photos.first { $0.url == victim })
        #expect(changed.rating == 0)
        model.selection = [changed.id]
        #expect(MissingActions.changed(model) == [changed.id])
        await model.restorePrevious([changed.id])
        let back = try #require(model.photos.first { $0.url == victim })
        #expect(!back.fileChanged && back.rating == 5)
        // Replace again and keep it as new.
        let other = try #require(model.photos.first { $0.url != victim }?.url)
        try FileManager.default.removeItem(at: victim)
        try FileManager.default.copyItem(at: other, to: victim)
        await model.openFolder(dir)
        #expect(await waitUntil(timeout: .seconds(20)) { model.photos.contains { $0.url == victim && $0.fileChanged } })
        let fresh = try #require(model.photos.first { $0.url == victim })
        await model.keepAsNew([fresh.id])
        #expect(model.photos.first { $0.url == victim }?.fileChanged == false)
        // Nothing kept in the store for this photo: the error is shown, nothing moves.
        await model.restoreToFolder([fresh.id])
        #expect(FileManager.default.fileExists(atPath: victim.path))
        #expect(model.toast?.isError == true)
    }

    @MainActor @Test func backgroundUpdatesRefreshTheCurrentSessionKeepingFocus() async throws {
        let model = makeModel(counts: [12])
        await model.start()
        let session = try #require(model.currentSession)
        try #require(await waitUntil { model.visibleIDs.count > 5 }, "session listed: \(model.visibleIDs.count)")
        let target = model.visibleIDs[5]
        model.focus(target, extend: false)
        // Not for another folder, and not while an open is in flight.
        model.receive(OpenProgress(phase: .updated, done: 1, total: 0), for: URL(fileURLWithPath: "/elsewhere"))
        #expect(model.openProgress == nil)
        model.receive(OpenProgress(phase: .updated, done: 1, total: 0), for: session.url)
        #expect(await waitUntil { model.focusedID == target && !model.isLoading })
        #expect(model.openProgress == nil, "no progress sheet for an update")
        #expect(OpenProgressText.phase(OpenProgress(phase: .hashing, done: 1, total: 4)).contains("25%"))
        #expect(OpenProgressText.phase(OpenProgress(phase: .updated, done: 1, total: 0)) == "Updated")
        // Background verification: shown while running, cleared when done or when
        // there was nothing to verify (the screenshot showed a stuck "0% (0 of 0)").
        model.receive(OpenProgress(phase: .hashing, done: 1, total: 4), for: session.url)
        #expect(model.openProgress?.done == 1)
        model.receive(OpenProgress(phase: .hashing, done: 4, total: 4), for: session.url)
        #expect(model.openProgress == nil)
        model.receive(OpenProgress(phase: .hashing, done: 0, total: 0), for: session.url)
        #expect(model.openProgress == nil)
    }

    @MainActor @Test func navigationAndRatingPauseBackgroundWork() async throws {
        let store = ActivityStore()
        let model = makeModel(store: store)
        await model.start()
        let before = store.count
        try #require(await waitUntil { model.visibleIDs.count > 1 })
        model.focus(model.visibleIDs[1], extend: false)
        model.setRating(3)
        #expect(store.count >= before + 2)
        // The live store forwards to Rust (no throw, safe from any thread).
        RustSessionStore(recents: recents(), storeBase: tempBase()).noteUserActivity()
    }

    @MainActor @Test func fileChangedAndStoreCopyActionsTargetTheRightPhotos() async throws {
        let store = ActivityStore()
        let model = makeModel(store: store)
        await model.start()
        let ids = Array(model.visibleIDs.prefix(3))
        model.updatePhotos([ids[0]], label: "t") { $0.fileChanged = true }
        model.updatePhotos([ids[1]], label: "t") { $0.copyInStore = true }
        model.selection = Set(ids)
        #expect(MissingActions.changed(model) == [ids[0]])
        #expect(MissingActions.restorable(model) == [ids[1]])
        #expect(MissingActions.restoreToFolder.label == "Restore to Folder")
        // Stores that can't restore report it instead of failing silently.
        let plain = FolderSessionStore(recents: recents(), isSupported: { _ in false }) { _, id in samplePhoto(id.raw) }
        await #expect(throws: (any Error).self) { try await plain.restorePrevious(ids[0]) }
        await #expect(throws: (any Error).self) { try await plain.restoreToFolder(ids[0]) }
        try await plain.dismissFileChanged(ids[0])
    }
}

/// A mock store that counts user-activity notes.
private final class ActivityStore: SessionStore, UserActivityObserving, @unchecked Sendable {
    let inner = MockSessionStore(counts: [10])
    private let lock = NSLock()
    private var n = 0
    var count: Int { lock.withLock { n } }
    func noteUserActivity() { lock.withLock { n += 1 } }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
}

private final class Locked<T>: @unchecked Sendable {
    private var value: T
    private let lock = NSLock()
    init(_ v: T) { value = v }
    func withLock<R>(_ f: (inout T) -> R) -> R { lock.withLock { f(&value) } }
}
