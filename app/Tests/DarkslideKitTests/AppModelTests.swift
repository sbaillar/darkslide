import CoreGraphics
import Foundation
import Synchronization
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@MainActor
@Suite struct AppModelTests {
    @Test func startOpensFirstSessionAndFocusesFirstPhoto() async {
        let m = makeModel()
        await m.start()
        #expect(m.sessionList.count == 2)
        #expect(m.currentSession?.name == "2026-09-21 Fall Hike")
        #expect(m.photos.count == 40)
        #expect(m.focusedID == m.visiblePhotos.first?.id)
        #expect(m.selection == [m.focusedID!])
        #expect(m.albums.map(\.name) == ["Selects", "Portfolio"])
        #expect(await waitUntil { m.viewer.frame != nil })
        #expect(await waitUntil { m.viewer.histogram?.pixelCount == 48 })
        m.stop()
    }

    @Test func onboardingSheetShowsOnFirstRun() {
        let m = makeModel(onboarded: false)
        #expect(m.sheet == .onboarding)
        #expect(makeModel().sheet == nil)
    }

    @Test func arrowsMoveFocusInFilmstripAndGrid() async {
        let m = makeModel()
        await m.start()
        let ids = m.visiblePhotos.map(\.id)
        m.perform(.move(.right, extend: false))
        #expect(m.focusedID == ids[1])
        m.perform(.move(.left, extend: false))
        m.perform(.move(.left, extend: false))
        #expect(m.focusedID == ids[0], "clamps at the start")
        m.perform(.move(.down, extend: false))
        #expect(m.focusedID == ids[1], "down = next in filmstrip")
        m.browserMode = .grid
        m.gridColumns = 5
        m.perform(.move(.down, extend: false))
        #expect(m.focusedID == ids[6])
        m.perform(.move(.up, extend: false))
        #expect(m.focusedID == ids[1])
        for _ in 0..<100 { m.perform(.move(.down, extend: false)) }
        #expect(m.focusedID == ids.last, "clamps at the end")
    }

    @Test func shiftArrowsExtendSelectionFromAnchor() async {
        let m = makeModel()
        await m.start()
        let ids = m.visiblePhotos.map(\.id)
        m.perform(.move(.right, extend: true))
        m.perform(.move(.right, extend: true))
        #expect(m.selection == Set(ids[0...2]))
        m.perform(.move(.left, extend: true))
        #expect(m.selection == Set(ids[0...1]))
        m.perform(.move(.right, extend: false))
        #expect(m.selection == [ids[2]])
        m.toggleSelection(ids[5])
        #expect(m.selection == [ids[2], ids[5]])
        #expect(m.focusedID == ids[5])
        m.toggleSelection(ids[5])
        #expect(m.selection == [ids[2]])
        m.perform(.selectAll)
        #expect(m.selection.count == 40)
        m.perform(.escape)
        #expect(m.selection == [m.focusedID!], "escape collapses the selection")
    }

    @Test func ratingTagsAndPicksAreUndoable() async {
        let m = makeModel()
        await m.start()
        let id = m.focusedID!
        let original = m.photo(id)!.rating
        m.perform(.rate(4))
        #expect(m.photo(id)?.rating == 4)
        m.perform(.tag(.green))
        #expect(m.photo(id)?.colorTag == .green)
        m.perform(.tag(.green))
        #expect(m.photo(id)?.colorTag == ColorTag.none, "same tag key toggles off")
        m.perform(.pick)
        #expect(m.photo(id)?.pick == .picked)
        m.perform(.reject)
        #expect(m.photo(id)?.pick == .rejected)
        m.perform(.unflag)
        #expect(m.photo(id)?.pick == PickState.none)
        m.perform(.rate(0))
        #expect(m.photo(id)?.rating == 0)
        #expect(m.history.undoLabel == "Clear Rating")

        m.perform(.undo)
        #expect(m.photo(id)?.rating == 4)
        m.perform(.undo) // unflag
        m.perform(.undo) // reject
        #expect(m.photo(id)?.pick == .picked)
        m.perform(.redo)
        #expect(m.photo(id)?.pick == .rejected)
        while m.history.canUndo { m.undo() }
        #expect(m.photo(id)?.rating == original)
        #expect(m.toast?.message.hasPrefix("Undo") == true)
    }

    @Test func ratingNoOpDoesNotRecordHistory() async {
        let m = makeModel()
        await m.start()
        m.setRating(3)
        let depth = m.history.undoStack.count
        m.setRating(3)
        #expect(m.history.undoStack.count == depth)
        m.setRating(9)
        #expect(m.focusedPhoto?.rating == 5, "clamped")
    }

    @Test func sliderDragCoalescesIntoOneUndoStep() async {
        let m = makeModel()
        await m.start()
        for v in stride(from: 0.1, through: 1.0, by: 0.1) {
            m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = v }
        }
        #expect(m.history.undoStack.count == 1)
        #expect(abs(m.focusedEdit.exposure.ev - 1.0) < 1e-9)
        #expect(m.focusedPhoto?.isEdited == true)
        m.undo()
        #expect(m.focusedEdit.isIdentity)
        #expect(m.focusedPhoto?.isEdited == false)
        m.redo()
        #expect(abs(m.focusedEdit.exposure.ev - 1.0) < 1e-9)
    }

    @Test func editAllSelectedAppliesToSelection() async {
        let m = makeModel()
        await m.start()
        m.perform(.move(.right, extend: true))
        m.perform(.move(.right, extend: true))
        m.editAllSelected = true
        m.updateEdit(label: "Contrast") { $0.exposure.contrast = 20 }
        #expect(m.selectedIDs.allSatisfy { m.edit(for: $0).exposure.contrast == 20 })
        m.editAllSelected = false
        m.updateEdit(label: "Contrast") { $0.exposure.contrast = 30 }
        #expect(m.selectedIDs.filter { m.edit(for: $0).exposure.contrast == 30 } == [m.focusedID!])
    }

    @Test func resetGroupRestoresDefaults() async {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "x") {
            $0.hdr.shadows = 40
            $0.exposure.ev = 1
            $0.layers = [LayerEdit(id: 0, name: "Sky")]
        }
        m.resetGroup(.hdr)
        #expect(m.focusedEdit.hdr == HDREdit())
        #expect(m.focusedEdit.exposure.ev == 1)
        m.resetGroup(.layers)
        #expect(m.focusedEdit.layers.isEmpty)
    }

    @Test func copyApplyAbsoluteAndRelativeAsOneStep() async {
        let m = makeModel()
        await m.start()
        let ids = m.visiblePhotos.map(\.id)
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.5 }
        m.copyAdjustments()
        #expect(m.clipboard?.sourceID == ids[0])
        m.focus(ids[1], extend: false)
        m.updateEdit(label: "ev") { $0.exposure.ev = 1.0 }
        let depth = m.history.undoStack.count
        let n = m.applyClipboard(to: [ids[1], ids[2]], options: PasteOptions(groups: [.exposure], relative: [.exposure]))
        #expect(n == 2)
        #expect(m.history.undoStack.count == depth + 1)
        #expect(m.edit(for: ids[1]).exposure.ev == 1.5)
        #expect(m.edit(for: ids[2]).exposure.ev == 0.5)
        m.undo()
        #expect(m.edit(for: ids[1]).exposure.ev == 1.0)
        #expect(m.edit(for: ids[2]).isIdentity)
        #expect(m.applyClipboard(to: [ids[0]], options: PasteOptions(groups: [.exposure])) == 0, "no-op paste")
        #expect(m.toast?.message == "Nothing to apply")
    }

    @Test func applyWithoutClipboardExplains() async {
        let m = makeModel()
        await m.start()
        m.perform(.applyAdjustments)
        #expect(m.sheet == nil)
        #expect(m.toast?.message.contains("Copy adjustments first") == true)
        #expect(m.applyClipboard(to: [m.focusedID!], options: PasteOptions()) == 0)
        m.perform(.copyAdjustments)
        m.perform(.applyAdjustments)
        #expect(m.sheet == nil, "no dialogs: Copy and Apply act at once")
    }

    @Test func copyFromPreviousCopiesEverything() async {
        let m = makeModel()
        await m.start()
        let ids = m.visiblePhotos.map(\.id)
        m.updateEdit(label: "wb") {
            $0.whiteBalance.asShot = false
            $0.whiteBalance.temperature = 4200
        }
        m.perform(.copyFromPrevious)
        #expect(m.history.undoLabel == "wb", "no previous image: nothing happens")
        m.perform(.move(.right, extend: false))
        m.perform(.copyFromPrevious)
        #expect(m.edit(for: ids[1]).whiteBalance.temperature == 4200)
        #expect(m.history.undoLabel == "Copy from Previous")
    }

    @Test func filterRebuildsVisibleAndIndex() async {
        let m = makeModel()
        await m.start()
        let target = m.visiblePhotos[3].id
        m.focus(target, extend: false)
        m.perform(.pick)
        m.filter = PhotoFilter(pick: .picked)
        #expect(m.visiblePhotos.map(\.id) == [target])
        #expect(m.visibleIndex(of: target) == 0)
        m.filter = PhotoFilter(pick: .rejected)
        #expect(m.visiblePhotos.isEmpty)
        m.filter = PhotoFilter()
        #expect(m.visiblePhotos.count == 40)
    }

    @Test func escapeUnwindsOverlaysInOrder() async {
        let m = makeModel()
        await m.start()
        m.perform(.showCheatSheet)
        m.perform(.toggleFullScreen)
        m.wbPickerActive = true
        m.perform(.escape)
        #expect(!m.showCheatSheet && m.viewer.state.fullScreen)
        m.perform(.escape)
        #expect(!m.viewer.state.fullScreen)
        m.perform(.escape)
        #expect(!m.wbPickerActive)
        m.perform(.gridMode)
        #expect(m.browserMode == .grid)
        m.perform(.viewerMode)
        #expect(m.browserMode == .filmstrip)
    }

    @Test func aiCullSuggestsPicksAsOneUndoStep() async {
        let m = makeModel()
        await m.start()
        await m.runAICull()
        #expect(m.cullProgress == nil)
        #expect(m.cullSuggestions.count == 40)
        let picked = m.photos.filter { $0.pick == .picked }.count
        let rejected = m.photos.filter { $0.pick == .rejected }.count
        #expect(picked + rejected > 0)
        #expect(m.history.undoLabel == "AI Cull")
        m.undo()
        #expect(m.photos.allSatisfy { $0.pick == PickState.none })
    }

    @Test func autoAdjustAndInstructionEditFocused() async {
        let m = makeModel()
        await m.start()
        await m.autoAdjust()
        #expect(m.focusedEdit.whiteBalance.asShot == false)
        #expect(m.history.undoLabel == "Auto Adjust")
        await m.applyInstruction("warmer")
        #expect(m.history.undoLabel == "Auto Adjust", "mock NL service changes nothing → no step")
    }

    @Test func archiveStatusStreamsIntoModel() async {
        let m = makeModel()
        await m.start()
        #expect(await waitUntil { if case .idle = m.archiveStatus.state { m.archiveStatus.lastSuccess != nil } else { false } })
        m.stop()
    }

    @Test func storeFailuresSurfaceAsToasts() async {
        let m = makeModel(store: FailingStore())
        await m.start()
        #expect(m.toast?.isError == true)
        #expect(m.currentSession == nil)
        await m.reloadSessions()
        #expect(m.toast?.message.contains("Not found") == true)
    }

    @Test func saveFailureIsPersistentNotAToast() async {
        let m = makeModel(store: SaveFailingStore())
        await m.start()
        #expect(m.photos.count == 3)
        m.setRating(2)
        m.updateEdit(label: "ev") { $0.exposure.ev = 1 }
        await m.saver.flush()
        #expect(m.saver.state == .failed("disk full", pending: 1), "M7: stays visible with a retry")
        #expect(m.toast == nil)
    }

    @Test func savesReachTheStore() async throws {
        let store = MockSessionStore(counts: [3])
        let m = makeModel(store: store)
        await m.start()
        m.setRating(2)
        m.updateEdit(label: "ev") { $0.exposure.ev = 1 }
        var saves = 0
        for _ in 0..<500 where saves < 2 {
            saves = await store.saveCount
            try await Task.sleep(for: .milliseconds(2))
        }
        #expect(saves == 2)
        let id = m.focusedID!
        let saved = try await store.edit(for: id)
        #expect(saved.exposure.ev == 1)
        let reloaded = try await store.photos(in: m.currentSession!)
        #expect(reloaded.first { $0.id == id }?.rating == 2)
    }

    @Test func reloadSessionsOpensTheNewOne() async {
        let store = MockSessionStore(counts: [3])
        let m = makeModel(store: store)
        await m.start()
        let s = try! await store.createSession(named: "Imported Today")
        await m.reloadSessions(select: s)
        #expect(m.sessionList.first?.name == "Imported Today")
        #expect(m.currentSession?.id == s.id)
        #expect(m.photos.isEmpty && m.focusedID == nil && m.viewer.frame == nil)
    }

    /// Deterministic (review M3): the toast's clock is injected; the expiry task is awaited.
    @Test func toastsExpire() async {
        let m = makeModel()
        let released = Mutex(false)
        m.toastSleep = { _ in
            while !released.withLock({ $0 }) { try await Task.sleep(for: .milliseconds(1)) }
        }
        m.show(Toast("first"))
        let first = m.toastTask
        m.show(Toast("hi"))
        await first?.value
        #expect(m.toast?.message == "hi", "a replaced toast's timer doesn't clear the new one")
        released.withLock { $0 = true }
        await m.toastTask?.value
        #expect(m.toast == nil)
    }

}
