import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Forwards to a RustSessionStore; the first rating write fails (a drive hiccup).
private actor FailOnce: SessionStore {
    let inner: RustSessionStore
    private var failed = false
    init(_ inner: RustSessionStore) { self.inner = inner }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws {
        if !failed {
            failed = true
            throw MockError.invalid("the drive went away for a moment")
        }
        try await inner.save(photos: photos)
    }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func edits(in session: SessionInfo) async throws -> [PhotoID: EditState] { try await inner.edits(in: session) }
}

/// A copied sample folder whose second original is gone (missing once reopened).
private func folderWithAMissingPhoto() throws -> (folder: URL, base: URL)? {
    guard let folder = try sampleFolderCopy(limit: 2) else { return nil }
    return (folder, folder.deletingLastPathComponent().appendingPathComponent("ds-save-\(UUID())"))
}

@MainActor
@Suite(.serialized) struct SaveRobustnessTests {
    /// The reviewer's repro: a pending rating for a photo Remove Missing just forgot
    /// must not block every other pending change for good.
    @Test func aRemovedPhotosPendingRatingDoesNotPoisonTheSaver() async throws {
        guard let (folder, base) = try folderWithAMissingPhoto() else { return }
        defer {
            try? FileManager.default.removeItem(at: folder)
            try? FileManager.default.removeItem(at: base)
        }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info = try await rust.add(folder: folder)
        var photos = try await rust.photos(in: info)
        #expect(photos.count == 2)
        let gone = try #require(photos[1].url)
        try FileManager.default.removeItem(at: gone)
        await rust.close(info)
        photos = try await rust.photos(in: info)
        let missing = try #require(photos.first { $0.isMissing })
        let kept = try #require(photos.first { !$0.isMissing })
        let saver = SaveCoordinator(store: rust, debounce: .seconds(60))
        var a = kept, b = missing
        a.rating = 4
        b.rating = 5
        saver.enqueue(photos: [a, b])
        try await rust.removeMissing([missing.id])
        await saver.flush()
        #expect(saver.state == .saved, "\(saver.state)")
        #expect(saver.dropped == 0, "a removed photo is skipped, not an error")
        let fresh = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let again = try await fresh.add(folder: folder)
        #expect(try await fresh.photos(in: again).first { $0.id == kept.id }?.rating == 4, "the valid change is written")
    }

    /// A saver already failed (a drive hiccup), then Remove Missing: the next flush
    /// writes what is left and recovers.
    @Test func aFailedSaverRecoversAfterRemoveMissing() async throws {
        guard let (folder, base) = try folderWithAMissingPhoto() else { return }
        defer {
            try? FileManager.default.removeItem(at: folder)
            try? FileManager.default.removeItem(at: base)
        }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info = try await rust.add(folder: folder)
        let first = try await rust.photos(in: info)
        try FileManager.default.removeItem(at: try #require(first[1].url))
        await rust.close(info)
        let photos = try await rust.photos(in: info)
        let missing = try #require(photos.first { $0.isMissing })
        let kept = try #require(photos.first { !$0.isMissing })
        let saver = SaveCoordinator(store: FailOnce(rust), debounce: .seconds(60))
        var a = kept, b = missing
        a.rating = 3
        b.rating = 2
        saver.enqueue(photos: [a, b])
        await saver.flush()
        guard case .failed(_, let pending) = saver.state else { Issue.record("\(saver.state)"); return }
        #expect(pending == 2)
        try await rust.removeMissing([missing.id])
        await saver.retry()
        #expect(saver.state == .saved, "\(saver.state)")
        #expect(try await rust.photos(in: info).first { $0.id == kept.id }?.rating == 3)
    }

    /// One batch: a photo of the open session, one Remove Missing forgot, one of a
    /// closed session. The first is written, the second skipped, the third fails loudly.
    @Test func aMixedBatchWritesWhatItCanAndNamesTheRest() async throws {
        guard let (a, base) = try folderWithAMissingPhoto(), let b = try sampleFolderCopy(limit: 2) else { return }
        defer {
            try? FileManager.default.removeItem(at: a)
            try? FileManager.default.removeItem(at: b)
            try? FileManager.default.removeItem(at: base)
        }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let infoA = try await rust.add(folder: a)
        let infoB = try await rust.add(folder: b)
        let pa = try await rust.photos(in: infoA)
        try FileManager.default.removeItem(at: try #require(pa[1].url))
        await rust.close(infoA)
        let reA = try await rust.photos(in: infoA)
        let missing = try #require(reA.first { $0.isMissing })
        try await rust.removeMissing([missing.id])
        let pb = try await rust.photos(in: infoB)
        await rust.close(infoA) // A is closed now; its photos are late writes
        let late = try #require(reA.first { !$0.isMissing })
        var x = pb[0], y = missing, z = late
        x.rating = 5
        y.rating = 5
        z.rating = 5
        do {
            try await rust.save(photos: [x, y, z])
            Issue.record("a closed session's photo must fail")
        } catch let e as UnsavableChanges {
            #expect(e.ids == [z.id])
            #expect(e.localizedDescription.contains("closed"))
        }
        #expect(try await rust.photos(in: infoB).first { $0.id == x.id }?.rating == 5, "written anyway")
        var edit = EditState()
        edit.exposure.ev = 0.5
        await #expect(throws: UnsavableChanges.self) { try await rust.save(edits: [x.id: edit, z.id: edit, y.id: edit]) }
        #expect(try await rust.edit(for: x.id).exposure.ev == 0.5)
        // Through the saver: the closed session's change is dropped (counted, said), the
        // rest saved, and the saver isn't stuck.
        let saver = SaveCoordinator(store: rust, debounce: .seconds(60))
        x.rating = 1
        saver.enqueue(photos: [x, z])
        await saver.flush()
        #expect(saver.state == .dropped(try #require(saver.lastDropMessage), count: 1) && saver.pendingCount == 0)
        #expect(saver.lastDropMessage?.contains("closed") == true)
        x.rating = 2
        saver.enqueue(photos: [x])
        await saver.flush()
        #expect(saver.dropped == 1 && !saver.hasUnsaved, "written; the notice stays until dismissed")
    }

    /// Remove Missing writes pending changes first (so none is left for a photo it forgets).
    @Test func removeMissingFlushesFirst() async throws {
        guard let (folder, base) = try folderWithAMissingPhoto() else { return }
        defer {
            try? FileManager.default.removeItem(at: folder)
            try? FileManager.default.removeItem(at: base)
        }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info = try await rust.add(folder: folder)
        let first = try await rust.photos(in: info)
        try FileManager.default.removeItem(at: try #require(first[1].url))
        await rust.close(info)
        let m = makeModel(store: rust)
        m.volumeProbe = { _, _ in VolumeCheck(mounted: true, sameVolume: true) }
        await m.open(info)
        let missing = try #require(m.photos.first { $0.isMissing })
        m.selectAll()
        m.setRating(4)
        #expect(m.saver.pendingCount > 0)
        await m.removeMissing([missing.id])
        #expect(m.saver.state == .saved && m.saver.dropped == 0, "\(m.saver.state)")
        #expect(m.photos.count == 1 && m.photos[0].rating == 4)
        m.stop()
    }
}

// MARK: - MN1: a hung identity pass never wedges Close or Open

private actor HungClose: SessionStore, SessionClosing {
    let inner = MockSessionStore(counts: [3, 2])
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func close(_ session: SessionInfo) async { try? await Task.sleep(for: .seconds(3600)) } // an SMB volume that hangs
}

@MainActor
@Suite(.serialized) struct HungCloseTests {
    @Test func aStoreCloseThatNeverReturnsDoesNotWedgeTheApp() async throws {
        let m = makeModel(store: HungClose())
        m.closeTailLimit = .milliseconds(200)
        await m.start()
        let other = try #require(m.sessionList.last)
        let clock = ContinuousClock()
        let start = clock.now
        await m.closeSession(imports: ImportRegistry())
        #expect(m.currentSession == nil && m.closing == nil)
        await m.open(other)
        #expect(m.currentSession?.id == other.id)
        #expect(clock.now - start < .seconds(30), "bounded, not stuck (the store sleeps an hour)")
        m.stop()
    }

    @Test func anOpenNeverWaitsLongOnAClose() async throws {
        let m = makeModel(store: HungClose())
        m.closeTailLimit = .seconds(3600) // the close itself stays stuck
        m.openWaitLimit = .milliseconds(200)
        await m.start()
        let other = try #require(m.sessionList.last)
        let close = Task { await m.closeSession(imports: ImportRegistry()) }
        #expect(await waitUntil { m.currentSession == nil })
        let clock = ContinuousClock()
        let start = clock.now
        await m.open(other)
        #expect(m.currentSession?.id == other.id)
        #expect(clock.now - start < .seconds(30), "bounded, not stuck (the store sleeps an hour)")
        close.cancel()
        m.stop()
    }

    /// The live store: closing doesn't wait for an identity pass that never returns.
    @Test func theRustStoreDoesNotWaitForAHungPass() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-hung-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: root.appendingPathComponent("s"),
                                     libraryRoot: { root.appendingPathComponent("lib") })
        let s = try await store.createSession(named: "Hung")
        await store.setPassWaitLimit(.milliseconds(200))
        await store.installHashingPass(for: s, Task { try? await Task.sleep(for: .seconds(3600)) })
        let clock = ContinuousClock()
        let start = clock.now
        await store.close(s)
        #expect(clock.now - start < .seconds(30), "bounded, not stuck (the store sleeps an hour)")
        #expect(await !store.isOpen(s))
    }
}
