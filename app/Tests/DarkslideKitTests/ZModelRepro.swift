import CoreGraphics
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Ported from the re-review's rr-model repros; each asserts the correct behaviour.
@MainActor @Suite(.serialized) struct ZModelRepro {
    // N2: filter + auto-advance must not skip a photo.
    @Test func filterAutoAdvanceSkips() async {
        let m = makeModel(counts: [40])
        m.settings.autoAdvance = true
        await m.start()
        m.filter.pick = .unrejected
        let ids = m.visibleIDs
        m.focus(ids[5], extend: false)
        m.perform(.reject)
        #expect(m.focusedID == ids[6], "the photo that took the rejected one's place is next, not ids[7]")
        m.filter = PhotoFilter(unratedOnly: true)
        let unrated = m.visibleIDs
        m.focus(unrated[2], extend: false)
        m.perform(.rate(3))
        #expect(m.focusedID == unrated[3], "culling Unrated with 1-5 visits every photo")
        m.filter = PhotoFilter()
        m.focus(m.visibleIDs[2], extend: false)
        let next = m.visibleIDs[3]
        m.perform(.rate(2))
        #expect(m.focusedID == next, "no filter: plain advance")
    }

    // N7: S on a non-top member keeps focus visible; → moves one step.
    @Test func stackCollapseHidesFocus() async {
        let m = makeModel(counts: [60])
        await m.start()
        m.browserState.refresh(photos: m.photos)
        guard let s = m.browserState.stacks.stacks.first(where: { $0.count > 1 && m.visibleIndex(of: $0.memberIDs.last!)! > 5 }) else {
            Issue.record("no stack")
            return
        }
        let member = s.memberIDs.first { $0 != s.topID }!
        m.focus(member, extend: false)
        m.perform(.toggleStack)
        let nav = m.navigationIDs
        #expect(nav.contains(m.focusedID!), "focus re-pointed to the stack's representative")
        let at = nav.firstIndex(of: m.focusedID!)!
        m.perform(.move(.right, extend: false))
        #expect(m.focusedID == nav[min(at + 1, nav.count - 1)], "→ steps from the stack, not from the start")
    }

    // N7: rejecting the top of a collapsed stack keeps focus in place.
    @Test func rejectTopOfCollapsedStack() async {
        let m = makeModel(counts: [60])
        await m.start()
        m.perform(.toggleStack)
        let st = m.browserState.stacks
        guard let s = st.stacks.first(where: { $0.count > 1 && m.visibleIndex(of: $0.memberIDs.first!)! > 5 }) else {
            Issue.record("no stack")
            return
        }
        let top = m.navigationIDs.first { s.memberIDs.contains($0) }!
        m.focus(top, extend: false)
        let navBefore = m.navigationIDs
        let at = navBefore.firstIndex(of: top)!
        m.perform(.reject)
        #expect(m.navigationIDs.contains(m.focusedID!), "focus stays on something shown")
        m.perform(.move(.right, extend: false))
        let pos = m.navigationIDs.firstIndex(of: m.focusedID!)!
        #expect(pos >= at, "→ continues from the stack (was \(at), now \(pos)), not index 1")
    }

    // N6a: session switch keeps stored edits.
    @Test func sessionSwitchLosesEdits() async throws {
        let m = makeModel(counts: [10, 5])
        await m.start()
        let a = m.currentSession!, b = m.sessionList.first { $0.id != a.id }!
        let id = m.focusedID!
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1.5 }
        await m.saver.flush()
        await m.open(b)
        await m.open(a)
        #expect(m.edit(for: id).exposure.ev == 1.5)
        #expect(m.photo(id)?.isEdited == true)
    }

    // N6b: reopening the same folder keeps ids, ratings and edits.
    @Test func folderReopenLosesRatings() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("zrepro-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        for i in 0..<4 { try Data([0]).write(to: dir.appendingPathComponent("DSC\(i).ARW")) }
        let store = FolderSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!),
                                       isSupported: { $0.pathExtension == "ARW" },
                                       loader: { url, id in
                                           var p = samplePhoto(id.raw)
                                           p.url = url
                                           p.filename = url.lastPathComponent
                                           return p
                                       })
        let m = makeModel(store: store)
        await m.start()
        await m.openFolder(dir)
        let first = m.focusedID!
        m.perform(.rate(4))
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        await m.saver.flush()
        try Data([0]).write(to: dir.appendingPathComponent("DSC9.ARW"))
        await m.openFolder(dir)
        #expect(m.visibleIDs.contains(first), "same id after reopen")
        #expect(m.photo(first)?.rating == 4)
        #expect(m.edit(for: first).exposure.ev == 1)
        #expect(m.visibleIDs.count == 5, "a new file is picked up")
    }

    // N4: hover preview is scoped to the photo it was made for.
    @Test func hoverPreviewLeaksToNextPhoto() async {
        let r = FakeRenderer()
        let m = makeModel(renderer: r)
        await m.start()
        _ = await waitUntil { !r.calls.isEmpty }
        var hovered = EditState()
        hovered.exposure.ev = 2
        m.viewer.setPreviewEdit(hovered)
        m.perform(.move(.right, extend: false))
        let next = m.focusedID!
        _ = await waitUntil { r.calls.contains { $0.0 == next } }
        let call = r.calls.last { $0.0 == next }!
        #expect(call.1.exposure.ev == m.edit(for: next).exposure.ev, "next photo renders its own edit")
    }

    // N10: Edit All Selected syncs global sliders but never per-image edits.
    @Test func editAllSelectedStraightensEveryone() async {
        let m = makeModel()
        await m.start()
        m.perform(.move(.right, extend: true))
        m.perform(.move(.right, extend: true))
        m.editAllSelected = true
        let sel = m.selectedIDs
        m.updateEdit(label: "Straighten") { $0.crop.angle = 3 }
        #expect(sel.filter { m.edit(for: $0).crop.angle != 0 } == [m.focusedID!], "straighten stays on the focused photo")
        m.updateEdit(label: "Add layer") { $0.layers.append(LayerEdit(id: 0, name: "L")) }
        #expect(sel.filter { !m.edit(for: $0).layers.isEmpty } == [m.focusedID!], "layer structure stays per image")
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 0.7 }
        #expect(sel.allSatisfy { m.edit(for: $0).exposure.ev == 0.7 }, "global sliders sync")
    }

    // N15: a command added after the user's first rebind still has its default key.
    @Test func bindingsMissingNewCommand() {
        var d = KeyBindings.defaults.dictionary
        d["toggleStack"] = nil
        let b = KeyBindings(dictionary: d)
        #expect(b.command(for: "s") == .toggleStack)
    }

    // L7: Copy from Previous keeps each photo's orientation.
    @Test func copyFromPreviousKeepsOrientation() async {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "rotate") {
            $0.crop.quarterTurns = 1
            $0.exposure.ev = 0.5
        }
        m.perform(.move(.right, extend: false))
        m.perform(.copyFromPrevious)
        #expect(m.focusedEdit.exposure.ev == 0.5 && m.focusedEdit.crop.quarterTurns == 0)
    }

    // L3: two quick drags of the same slider are two undo steps.
    @Test func undoPerGesture() async {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.3 }
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.6 }
        m.endEditGesture()
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.9 }
        m.endEditGesture()
        #expect(m.history.undoStack.count == 2)
        m.undo()
        #expect(m.focusedEdit.exposure.ev == 0.6)
    }

    // N5: quitting reports what would be lost.
    @Test func terminationDecisions() async {
        let ok = makeModel()
        await ok.start()
        ok.setRating(3)
        // Only the `slow` case below tests the timeout; the others get a generous one so a
        // saturated machine (parallel suites, cargo builds) can't turn a flush into
        // "Saving is taking too long" (the 1-in-2 failure at the `.unsavedInMemory` check).
        let generous = Duration.seconds(120)
        let okDecision = await ok.prepareToTerminate(timeout: generous)
        #expect(okDecision == .ok, "persistent store: flushed, safe to quit: \(okDecision)")
        let failing = makeModel(store: SaveFailingStore())
        await failing.start()
        failing.setRating(2)
        if case .failed(let msg, let n) = await failing.prepareToTerminate(timeout: generous) {
            #expect(msg == "disk full" && n == 1)
        } else { Issue.record("expected failed") }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("zterm-\(UUID())")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try? Data([0]).write(to: dir.appendingPathComponent("a.ARW"))
        let store = FolderSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!),
                                       isSupported: { $0.pathExtension == "ARW" },
                                       loader: { url, id in
                                           var p = samplePhoto(id.raw)
                                           p.url = url
                                           return p
                                       })
        let mem = makeModel(store: store)
        await mem.start()
        await mem.openFolder(dir)
        let unchanged = await mem.prepareToTerminate(timeout: generous)
        #expect(unchanged == .ok, "nothing changed yet: \(unchanged)")
        mem.setRating(5)
        let changed = await mem.prepareToTerminate(timeout: generous)
        #expect(changed == .unsavedInMemory(count: 1), "\(changed)")
        let slow = makeModel(store: SlowStore())
        await slow.start()
        slow.setRating(1)
        let t0 = ContinuousClock.now
        if case .failed = await slow.prepareToTerminate(timeout: .milliseconds(50)) {} else { Issue.record("expected timeout") }
        // It returns at the timeout, not when the 60 s save finally ends (bound loose for loaded machines).
        #expect(ContinuousClock.now - t0 < .seconds(30), "\(ContinuousClock.now - t0)")
    }

    // L4: concurrent flushes are serialized and in order.
    @Test func flushesAreSerialized() async {
        let store = OrderRecordingStore()
        let s = SaveCoordinator(store: store, debounce: .seconds(60))
        var a = samplePhoto(1)
        a.rating = 1
        s.enqueue(photos: [a])
        let f1 = Task { await s.flush() }
        // Make sure the first write is in flight before the second change arrives
        // (otherwise both land in one batch, whose internal order is unspecified).
        while !store.started { try? await Task.sleep(for: .milliseconds(1)) }
        var b = samplePhoto(2)
        b.rating = 2
        s.enqueue(photos: [b])
        let f2 = Task { await s.flush() }
        _ = await (f1.value, f2.value)
        #expect(store.ratings == [1, 2], "writes land in order")
        #expect(s.state == .saved)
    }
}

/// Every write takes 10 s (quit timeout path).
struct SlowStore: SessionStore {
    let inner = MockSessionStore(counts: [3])
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await Task.sleep(for: .seconds(60)) }
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
}

final class OrderRecordingStore: SessionStore, @unchecked Sendable {
    var ratings: [Int] = []
    var started = false
    func listSessions() async throws -> [SessionInfo] { [] }
    func photos(in session: SessionInfo) async throws -> [Photo] { [] }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("no") }
    func save(photos: [Photo]) async throws {
        started = true
        try await Task.sleep(for: .milliseconds(photos.first?.rating == 1 ? 30 : 1))
        ratings += photos.map(\.rating)
    }
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
}
