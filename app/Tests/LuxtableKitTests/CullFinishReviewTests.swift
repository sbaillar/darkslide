import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Finishing a cull, from the adversarial review (H1, M6, M7, M8, L1): a picked
/// variant never goes to the Trash with a rejected sibling, Delete Rejected counts
/// files and never deletes outright, undo puts photos back where they were, and
/// moves keep the edit history.
@MainActor @Suite(.serialized) struct CullFinishReviewTests {
    func model(_ store: Cul9Store) async -> AppModel {
        let m = makeModel(store: store)
        await m.start()
        return m
    }

    /// H1 (fake store): a photo with a picked and a rejected variant keeps its file;
    /// only the rejected variant is hidden. ⌘Z brings it back.
    @Test func aPickedVariantKeepsItsFile() async throws {
        let store = Cul9Store(count: 2)
        let m = await model(store)
        let original = m.visibleIDs[0]
        m.focus(original, extend: false)
        await m.cull.newVariant()
        let variant = try #require(m.focusedID)
        m.setPick(.picked)
        m.focus(original, extend: false)
        m.setPick(.rejected)
        #expect(m.cull.rejectedFileCount == 0, "the file has a kept variant")
        await m.cull.moveRejectedToTrash()
        #expect(m.photo(variant)?.inTrash == false && m.photo(variant)?.pick == .picked)
        #expect(m.visibleIDs.contains(variant) && m.photo(original) == nil)
        #expect(m.cull.trashedIDs.isEmpty)
        #expect(m.toast?.message == "Hid 1 rejected variant of photos you kept (⌘Z to undo)")
        m.cull.requestDeleteRejected()
        #expect(m.cull.pendingDelete == nil, "no file to delete")
        m.undo()
        #expect(await waitUntil { m.photo(original) != nil })
        #expect(m.photo(original)?.pick == .rejected && m.photo(variant)?.pick == .picked)
    }

    /// L1: Delete Rejected… counts files: two rejected variants of one file are one.
    @Test func deleteRejectedCountsFiles() async throws {
        let store = Cul9Store(count: 3)
        let m = await model(store)
        let a = m.visibleIDs[0]
        m.focus(a, extend: false)
        await m.cull.newVariant()
        let a2 = try #require(m.focusedID)
        m.setPick(.rejected)
        m.focus(a, extend: false)
        m.setPick(.rejected)
        #expect(m.cull.rejectedIDs.count == 2 && m.cull.rejectedFileCount == 1)
        m.cull.requestDeleteRejected()
        #expect(m.cull.pendingDelete == 1)
        #expect(CullFinisher.deleteMessage(1).hasPrefix("1 rejected file goes to the macOS Trash"))
        #expect(CullFinisher.deleteMessage(3).hasPrefix("3 rejected files go"))
        await m.cull.confirmDeleteRejected()
        #expect(await store.trashEmptied == 1 && m.photo(a) == nil && m.photo(a2) == nil)
        #expect(m.toast?.message == "Moved 1 rejected file to the Trash")
    }

    /// M6: the Trash refuses (a volume without one): an error, nothing deleted.
    @Test func aRefusingTrashDeletesNothing() async {
        let store = Cul9Store(count: 3)
        await store.setTrashRefuses(true)
        let m = await model(store)
        let a = m.visibleIDs[0]
        m.focus(a, extend: false)
        m.setPick(.rejected)
        m.cull.requestDeleteRejected()
        await m.cull.confirmDeleteRejected()
        #expect(m.toast?.isError == true && m.toast?.message.contains("nothing was deleted") == true)
        #expect(m.photo(a) != nil && m.photos.count == 3)
        #expect(await store.trashEmptied == 0)
    }

    /// M7 (fake store): a Select trashed, then ⌘Z: back in Selects.
    @Test func undoReturnsATrashedSelectToSelects() async {
        let store = Cul9Store(count: 3)
        let m = await model(store)
        let s = m.visibleIDs[1]
        m.focus(s, extend: false)
        m.setPick(.picked)
        await m.cull.movePickedToSelects()
        m.focus(s, extend: false)
        m.setPick(.rejected)
        await m.cull.moveRejectedToTrash()
        #expect(m.photo(s)?.inTrash == true)
        m.undo()
        #expect(await waitUntil { m.photo(s)?.inTrash == false })
        #expect(m.photo(s)?.inSelects == true)
    }

    /// M8: a move keeps the edit history: ⌘Z undoes the move, then the edits before it.
    @Test func movesKeepTheUndoHistory() async {
        let store = Cul9Store(count: 3)
        let m = await model(store)
        let ids = m.visibleIDs
        m.focus(ids[2], extend: false)
        m.setRating(4)
        m.focus(ids[0], extend: false)
        m.setPick(.rejected)
        await m.cull.moveRejectedToTrash()
        #expect(m.history.undoStack.count == 2, "\(m.history.undoStack.map(\.label))")
        // The kept steps describe where the photo is now.
        #expect(m.history.undoStack.last?.photosAfter[ids[0]]?.inTrash == true)
        m.undo() // the move
        #expect(await waitUntil { m.photo(ids[0])?.inTrash == false })
        m.undo() // the reject (waits for the move back when it's still finishing)
        #expect(await waitUntil { m.photo(ids[0])?.pick == PickState.none && m.photo(ids[0])?.inTrash == false })
        m.undo() // the rating
        #expect(await waitUntil { m.photo(ids[2])?.rating == 0 })
    }

    /// History rebase: file fields follow the listing; photos that left are dropped.
    @Test func historyRebaseFollowsFilesAndDropsGonePhotos() {
        var h = EditHistory()
        var before = samplePhoto(1)
        var after = before
        after.rating = 3
        var step = HistoryStep(label: "Rate")
        step.photosBefore = [before.id: before]
        step.photosAfter = [after.id: after]
        h.record(step)
        var gone = HistoryStep(label: "Gone")
        gone.editsAfter = [PhotoID(9): EditState()]
        gone.photosAfter = [PhotoID(9): samplePhoto(9)]
        h.record(gone)
        before.url = URL(fileURLWithPath: "/s/Trash/DSC01.ARW")
        before.inTrash = true
        h.rebase(onto: [before.id: before])
        #expect(h.undoStack.map(\.label) == ["Rate"])
        #expect(h.undoStack[0].photosAfter[before.id]?.inTrash == true)
        #expect(h.undoStack[0].photosAfter[before.id]?.rating == 3)
        #expect(h.undoStack[0].photosBefore[before.id]?.url?.path == "/s/Trash/DSC01.ARW")
    }

    @Test func trashedMessages() {
        #expect(CullFinisher.trashedMessage(files: 2, variants: 0) == "Moved 2 rejected photos to the session Trash (⌘Z to undo)")
        #expect(CullFinisher.trashedMessage(files: 1, variants: 2)
            == "Moved 1 rejected photo to the session Trash and hid 2 rejected variants of photos you kept (⌘Z to undo)")
    }

    // MARK: - Live (imported session, real files)

    /// H1 live: import, ⌘D, pick the new variant, reject the original, Move Rejected
    /// to Trash: the file stays in Capture, the picked variant stays visible, picked,
    /// the rejected variant is hidden. (That it stays archived is
    /// asserted in lt-ffi: `trash_rejected_keeps_picked_siblings_and_undo_restores_folders`.) ⌘Z brings it back.
    @Test func rejectingOneVariantKeepsThePickedSiblingLive() async throws {
        try await Imp6LiveImportTests.withEnv { env in
            var opened: SessionInfo?
            let im = Imp6LiveImportTests().importModel(env, service: env.service(), ejector: FakeEjector()) { opened = $0 }
            await im.scan()
            im.sessionText = "Variants"
            im.start()
            await im.wait()
            let session = try #require(opened)
            let m = makeModel(store: env.store)
            await m.open(session)
            let stills = m.visibleIDs.filter { m.photo($0)?.isVideo == false }
            let original = try #require(stills.first)
            let file = try #require(m.photo(original)?.url)
            m.focus(original, extend: false)
            await m.cull.newVariant()
            let variant = try #require(m.focusedID)
            #expect(variant != original)
            m.setPick(.picked)
            m.focus(original, extend: false)
            m.setPick(.rejected)
            #expect(m.cull.rejectedFileCount == 0)
            await m.cull.moveRejectedToTrash()
            let v = try #require(m.photo(variant))
            #expect(!v.inTrash && v.pick == .picked && m.visibleIDs.contains(variant))
            #expect(v.url?.path.contains("/Capture/") == true && FileManager.default.fileExists(atPath: file.path))
            #expect(m.photo(original) == nil, "the rejected variant is hidden")
            #expect(m.cull.trashedIDs.isEmpty)
            m.cull.requestDeleteRejected()
            #expect(m.cull.pendingDelete == nil)
            m.undo()
            #expect(await waitUntil { m.photo(original) != nil })
            #expect(m.photo(original)?.pick == .rejected && m.photo(variant)?.pick == .picked)
        }
    }

    /// M7 live: pick → Move to Selects → reject → Move Rejected to Trash → ⌘Z: the
    /// photo is back in Selects under its name.
    @Test func undoOfATrashedSelectReturnsItToSelectsLive() async throws {
        try await Imp6LiveImportTests.withEnv { env in
            var opened: SessionInfo?
            let im = Imp6LiveImportTests().importModel(env, service: env.service(), ejector: FakeEjector()) { opened = $0 }
            await im.scan()
            im.sessionText = "Selects"
            im.start()
            await im.wait()
            let session = try #require(opened)
            let m = makeModel(store: env.store)
            await m.open(session)
            let s = try #require(m.visibleIDs.first { m.photo($0)?.isVideo == false })
            m.focus(s, extend: false)
            m.setRating(2)
            m.setPick(.picked)
            await m.cull.movePickedToSelects()
            let selected = try #require(m.photo(s)?.url)
            #expect(m.photo(s)?.inSelects == true)
            m.focus(s, extend: false)
            m.setPick(.rejected)
            await m.cull.moveRejectedToTrash()
            #expect(m.photo(s)?.inTrash == true)
            // Two ⌘Zs at once: the second waits for the move back, then undoes the reject
            // (M8: the edits before the moves are still undoable).
            m.undo()
            m.undo()
            #expect(await waitUntil { m.photo(s)?.inTrash == false && m.photo(s)?.pick == .picked })
            #expect(m.photo(s)?.inSelects == true && m.photo(s)?.url == selected)
            #expect(FileManager.default.fileExists(atPath: selected.path))
        }
    }
}
