import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// The AI Changes list: its texts, undoing one or every AI change, and keeping the changes
/// in the sidecar across a relaunch.
@MainActor
@Suite struct AIChangesTests {
    static let ev = AIField(.exposure, "ev")
    static let contrast = AIField(.exposure, "contrast")
    static let vibrance = AIField(.presence, "vibrance")
    static let temperature = AIField(.whiteBalance, "temperature")
    static let tint = AIField(.whiteBalance, "tint")

    /// A model with the focused photo changed by an AI step (`ai`) from its current edit.
    static func model(_ store: MockSessionStore = MockSessionStore(counts: [6])) async -> AppModel {
        let m = makeModel(store: store)
        m.toastDuration = .seconds(60)
        await m.start()
        return m
    }

    static func aiStep(_ m: AppModel, _ source: AIMarks.Source = .auto, ids: [PhotoID]? = nil,
                       _ mutate: @escaping (inout EditState) -> Void) {
        m.updateEdits(ids ?? [m.focusedID!], label: "AI", ai: source) { _, e in mutate(&e) }
    }

    // MARK: Texts

    @Test func hoverNotesSayWhatAIChangedItToAndFrom() {
        var before = EditState()
        before.exposure.ev = 0
        var now = before
        now.exposure.ev = 0.35
        let c = AIChange(source: .optimize, explanation: "it was dark", time: Date(), before: before)
        #expect(AIChangeText.tooltip(Self.ev, c, now: now) == "AI changed it to +0.35 (was 0.00) — AI Optimize: it was dark")
        #expect(AIChangeText.detail(Self.ev, c, now: now) == "0.00 → +0.35")
        // Without an explanation, or an earlier value.
        let bare = AIChange(source: .optimize)
        #expect(AIChangeText.tooltip(Self.ev, bare, now: now) == "AI changed it to +0.35 — AI Optimize")
        #expect(AIChangeText.detail(Self.ev, bare, now: now) == "? → +0.35")
        // White balance: Kelvin, and As Shot.
        now.whiteBalance.asShot = false
        now.whiteBalance.temperature = 6200
        let wb = AIChange(source: .command("warmer"), before: before)
        #expect(AIChangeText.tooltip(Self.temperature, wb, now: now) == "AI changed it to \(Specs.temperature.string(6200)) (was As Shot) — ⌘K: “warmer”")
        // Degrees, and a percent.
        now.crop.angle = -1.8
        #expect(AIChangeText.tooltip(AIField(.crop, "angle"), AIChange(source: .auto, before: before), now: now)
            == "AI changed it to \(Specs.straighten.string(-1.8)) (was \(Specs.straighten.string(0))) — Auto Adjust")
        #expect(Specs.straighten.string(-1.8).hasSuffix("°"))
        now.vignette.midpoint = 0.7
        #expect(AIChangeText.detail(AIField(.vignette, "midpoint"), AIChange(source: .auto, before: before), now: now) == "50 % → 70 %")
    }

    @Test func rowsWithoutOneValueAreDescribed() {
        let before = EditState()
        var now = before
        now.noise.aiDenoise = true
        let c = AIChange(source: .auto, before: before)
        #expect(AIChangeText.tooltip(AIFieldMap.aiDenoise, c, now: now) == "AI changed it to On (was Off) — Auto Adjust")
        #expect(AIChangeText.detail(AIFieldMap.aiDenoise, c, now: now) == "Off → On")
        #expect(AIFieldMap.entry(AIFieldMap.lensEnabled)?.phrase(before: before, after: now) == "turned lens correction on")
        now.crop.aspect = .r4x5
        #expect(AIChangeText.tooltip(AIFieldMap.cropFrame, c, now: now) == "AI changed the crop — Auto Adjust")
        #expect(AIChangeText.detail(AIFieldMap.cropFrame, c, now: now) == "Changed the crop")
        now.layers = [LayerEdit(id: 7, name: "Sky")]
        #expect(AIChangeText.tooltip(AIFieldMap.layers, AIChange(source: .optimize, explanation: "dull sky", before: before), now: now)
            == "AI added a sky layer — AI Optimize: dull sky")
        now.layers = [LayerEdit(id: 7, name: "Easy light")]
        #expect(AIChangeText.detail(AIFieldMap.layers, c, now: now) == "Added an easy light layer")
        // A layer changed, none added; earlier value not known.
        #expect(AIChangeText.detail(AIFieldMap.layers, AIChange(source: .auto), now: now) == "Changed the layers")
        #expect(AIChangeText.detail(AIFieldMap.curve, c, now: now) == "Changed the curve")
        #expect(AIFieldMap.entry(Self.ev)?.phrase(before: nil, after: now) == "changed Exposure")
        // A row this version doesn't know.
        let unknown = AIField(.exposure, "future")
        #expect(AIChangeText.tooltip(unknown, c, now: now) == "Changed by AI — Auto Adjust")
        #expect(AIChangeText.detail(unknown, c, now: now) == "Changed")
    }

    @Test func whenAndSources() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        #expect(AIChangeText.when(nil, now: now) == "earlier")
        #expect(AIChangeText.when(now.addingTimeInterval(-10), now: now) == "just now")
        #expect(AIChangeText.when(now.addingTimeInterval(-300), now: now) == "5 min ago")
        #expect(AIChangeText.when(now.addingTimeInterval(-7300), now: now) == "2 h ago")
        #expect(!AIChangeText.when(now.addingTimeInterval(-90000), now: now).contains("ago"))
        #expect(AIChangeText.source(.optimize) == "AI Optimize")
        #expect(AIChangeText.source(.command("warmer")) == "⌘K “warmer”")
        #expect(AIChangeText.source(.auto) == "Auto Adjust")
    }

    /// Every row has a label, restores exactly its own values, and its value text (if any)
    /// is what its slider shows.
    @Test func everyRowRestoresItsOwnValues() {
        var changed = EditState()
        changed.whiteBalance.asShot = false
        changed.whiteBalance.temperature = 7000
        changed.whiteBalance.tint = 20
        changed.exposure.ev = 1
        changed.exposure.contrast = 10
        changed.levels.inBlack = 0.1
        changed.noise.aiDenoise = true
        changed.lens.enabled = false
        changed.lens.chromaticAberration = false
        changed.crop.quarterTurns = 1
        changed.colorEditor.bands[2].saturation = 30
        changed.layers = [LayerEdit(id: 1, name: "Sky")]
        for e in AIFieldMap.entries {
            #expect(!e.label.isEmpty)
            var x = changed
            e.restore(&x, EditState())
            #expect(!e.changed(x, EditState()), "\(e.field) is back")
            // Only this row's values (and, for white balance, As Shot) moved.
            let others = AIFieldMap.changed(changed, x).subtracting([e.field])
            if e.field.panel == .whiteBalance {
                #expect(others.isSubset(of: [Self.temperature, Self.tint]))
            } else {
                #expect(others.isEmpty, "\(e.field) restored \(others)")
            }
        }
        #expect(AIFieldMap.entry(AIField(.colorEditor, "band2.saturation"))?.label == "Yellow Saturation")
    }

    // MARK: The list

    @Test func theListShowsEachChangeInSidebarOrder() async throws {
        let m = await Self.model()
        Self.aiStep(m, .command("punchy")) {
            $0.presence.vibrance = 20
            $0.exposure.ev = 0.5
            $0.noise.aiDenoise = true
        }
        let rows = m.focusedAIChangeRows
        #expect(rows.map(\.field) == [Self.ev, Self.vibrance, AIFieldMap.aiDenoise])
        let ev = try #require(rows.first)
        #expect(ev.tool == "Exposure" && ev.setting == "Exposure" && ev.detail == "0.00 → +0.50")
        #expect(ev.source == "⌘K “punchy”" && ev.when == "just now" && ev.canUndo)
        #expect(ev.tooltip == "AI changed it to +0.50 (was 0.00) — ⌘K: “punchy”")
        #expect(ev.accessibilityLabel == "Exposure, Exposure: 0.00 → +0.50, by ⌘K “punchy”, just now")
        #expect(rows[1].tool == ToolPanelID.presence.title && rows[1].setting == "Vibrance")
        #expect(rows[2].detail == "Off → On")
        // A row the user changes by hand leaves the list.
        m.updateEdit(label: "Vibrance") { $0.presence.vibrance = 5 }
        #expect(m.focusedAIChangeRows.map(\.field) == [Self.ev, AIFieldMap.aiDenoise])
        // No focus, no rows.
        m.focus(nil, extend: false)
        #expect(m.focusedAIChangeRows.isEmpty)
    }

    @Test func undoingOneChangeKeepsTheOthersAndLaterManualEdits() async throws {
        let m = await Self.model()
        let id = try #require(m.focusedID)
        m.updateEdit(label: "Contrast") { $0.exposure.contrast = 15 } // the user's, before AI
        Self.aiStep(m) {
            $0.exposure.ev = 0.4
            $0.exposure.contrast = 30
            $0.presence.vibrance = 12
        }
        m.updateEdit(label: "Shadows") { $0.hdr.shadows = 25 } // the user's, after AI
        let steps = m.history.undoStack.count
        m.undoAIChanges([Self.contrast])
        #expect(m.history.undoStack.count == steps + 1, "one undo step")
        #expect(m.history.undoLabel == "Revert AI Change to Contrast")
        #expect(m.edit(for: id).exposure.contrast == 15, "back to the user's value before AI")
        #expect(m.edit(for: id).exposure.ev == 0.4 && m.edit(for: id).presence.vibrance == 12)
        #expect(m.edit(for: id).hdr.shadows == 25, "the later manual edit stays")
        #expect(m.aiMarks[id]?.fields == [Self.ev, Self.vibrance])
        // ⌘Z brings the AI value and its outline back.
        m.undo()
        #expect(m.edit(for: id).exposure.contrast == 30 && m.aiMarks[id]?.fields.contains(Self.contrast) == true)
        m.redo()
        #expect(m.edit(for: id).exposure.contrast == 15)
        // The context menu's Undo AI Change does the same; none for a row AI didn't change.
        #expect(m.aiUndo(Self.contrast) == nil)
        let undoEV = try #require(m.aiUndo(Self.ev))
        undoEV()
        #expect(m.edit(for: id).exposure.ev == 0 && m.aiMarks[id]?.fields == [Self.vibrance])
        // Undoing nothing is not a step.
        let count = m.history.undoStack.count
        m.undoAIChanges([Self.contrast])
        #expect(m.history.undoStack.count == count)
    }

    @Test func undoAllPutsEveryAIChangeBackAsOneStep() async throws {
        let m = await Self.model()
        let id = try #require(m.focusedID)
        #expect(!m.canUndoAllAIChanges && m.undoAllAIChangesTitle == "Undo All AI Changes")
        Self.aiStep(m, .optimize) { $0.exposure.ev = 0.45 }
        Self.aiStep(m, .auto) {
            $0.exposure.ev = 0.3
            $0.whiteBalance.asShot = false
            $0.whiteBalance.temperature = 5200
        }
        #expect(m.canUndoAllAIChanges)
        await m.undoAllAIChanges()
        #expect(m.edit(for: id) == EditState(), "every row back before the first AI step")
        #expect(m.aiMarks[id] == nil && m.history.undoLabel == "Revert 3 AI Changes")
        m.undo()
        #expect(m.edit(for: id).exposure.ev == 0.3 && m.aiMarks[id]?.fields.count == 3)
    }

    /// With Edit All Selected, Undo All puts back each selected photo's own AI changes.
    @Test func undoAllWithEditAllSelectedUndoesEachPhotosOwn() async throws {
        let m = await Self.model()
        let (a, b, c) = (m.visibleIDs[0], m.visibleIDs[1], m.visibleIDs[2])
        Self.aiStep(m, ids: [a]) { $0.exposure.ev = 0.5 }
        Self.aiStep(m, ids: [b]) { $0.presence.vibrance = 20 }
        m.updateEdits([b], label: "Mine") { _, e in e.exposure.ev = -0.2 } // b's own exposure
        Self.aiStep(m, ids: [c]) { $0.exposure.contrast = 10 }
        m.focus(a, extend: false)
        m.selection = [a, b]
        #expect(m.undoAllAIChangesTitle == "Undo All AI Changes", "Edit All Selected is off")
        m.editAllSelected = true
        #expect(m.undoAllAIChangesTitle == "Undo All AI Changes on 2 Photos")
        await m.undoAllAIChanges()
        #expect(m.edit(for: a).exposure.ev == 0)
        #expect(m.edit(for: b).presence.vibrance == 0 && m.edit(for: b).exposure.ev == -0.2)
        #expect(m.edit(for: c).exposure.contrast == 10, "not selected")
        #expect(m.history.undoLabel == "Revert 2 AI Changes")
        m.undo()
        #expect(m.edit(for: a).exposure.ev == 0.5 && m.edit(for: b).presence.vibrance == 20)
    }

    @Test func rowsFromAnOldReportCantBeUndoneHere() throws {
        let report = try OptimizeReport(json: ReportJSON.typical)
        let (edit, marked) = try RustPhotoOptimizer().apply(report, selected: [0], to: EditState())
        let marks = try #require(AIMarks(report: marked, stillApplied: [0], edit: edit))
        #expect(marks.changes.values.allSatisfy { $0.before == nil && $0.time == nil })
        #expect(AIChangesControl.undo(AIChangeRow(field: Self.ev, tool: "Exposure", setting: "Exposure", detail: "",
                                                  source: "AI Optimize", when: "earlier", tooltip: "", canUndo: false))
            .hint == AIChangeText.cantUndo)
        // A later AI step keeps the unknown earlier value; a step putting it back doesn't drop it.
        var after = edit
        after.exposure.ev = 0.1
        let more = try #require(AIMarks.adding(to: marks, source: .auto, before: edit, after: after, report: nil, time: Date()))
        #expect(more.changes[Self.ev]?.before == nil && more.changes[Self.ev]?.source == .auto)
        #expect(more.pruned(against: EditState()) == more)
    }

    // MARK: Commands, accessibility, voice

    @Test func menuCommandsOpenTheListAndUndoAll() async throws {
        let m = await Self.model()
        #expect(MenuCommand.aiChanges.shortcut == MenuCommand.Shortcut(key: "a", modifiers: [.command, .option, .shift]))
        #expect(MenuCommand.undoAllAIChanges.shortcut == nil)
        #expect(CommandCatalog.title(.aiChanges, model: m) == "AI Changes…")
        #expect(CommandCatalog.isEnabled(.aiChanges, model: m))
        CommandCatalog.perform(.aiChanges, model: m)
        #expect(m.aiChangesShown && CommandCatalog.title(.aiChanges, model: m) == "Hide AI Changes")
        m.focus(nil, extend: false)
        #expect(CommandCatalog.isEnabled(.aiChanges, model: m), "it can always be closed")
        CommandCatalog.perform(.aiChanges, model: m)
        #expect(!m.aiChangesShown && !CommandCatalog.isEnabled(.aiChanges, model: m))
        m.focus(m.visibleIDs[0], extend: false)
        #expect(!CommandCatalog.isEnabled(.undoAllAIChanges, model: m))
        Self.aiStep(m) { $0.exposure.ev = 1 }
        #expect(CommandCatalog.isEnabled(.undoAllAIChanges, model: m))
        #expect(CommandCatalog.title(.undoAllAIChanges, model: m) == "Undo All AI Changes")
        // Shortcuts stay unique.
        let shortcuts = MenuCommand.allCases.compactMap(\.shortcut)
        #expect(shortcuts.filter { $0 == MenuCommand.aiChanges.shortcut }.count == 1)
    }

    @Test func controlsAreLabelledAndVoiceOpensTheList() async throws {
        let m = await Self.model()
        Self.aiStep(m) { $0.exposure.ev = 1 }
        let row = try #require(m.focusedAIChangeRows.first)
        for c in [AIChangesControl.close, AIChangesControl.undoAll(m), AIChangesControl.row(row), AIChangesControl.undo(row)] {
            #expect(!c.label.isEmpty && !(c.hint ?? "").isEmpty, "\(c.id)")
        }
        #expect(AIChangesControl.undo(row).label == "Undo AI change to Exposure")
        #expect(AIChangesControl.undo(row).hint == "Put Exposure back to its value before AI")
        #expect(VoiceCommandParser.parse("ai changes") == [.menu(.aiChanges)])
        #expect(VoiceCommandParser.parse("show ai changes") == [.menu(.aiChanges)])
        m.toggleAIChangesList()
        #expect(m.aiChangesShown)
    }

    // MARK: Persistence

    @Test func recordsRoundTripGroupedByStep() throws {
        var before = EditState()
        before.exposure.contrast = 15
        before.whiteBalance.asShot = true
        before.crop.aspect = .r4x5
        let t1 = Date(timeIntervalSince1970: 1_700_000_000)
        let t2 = t1.addingTimeInterval(60)
        let marks = AIMarks(changes: [
            Self.ev: AIChange(source: .optimize, explanation: "dark", time: t1, before: before),
            Self.vibrance: AIChange(source: .optimize, explanation: "dull", time: t1, before: before),
            Self.temperature: AIChange(source: .command("warmer"), time: t2, before: EditState()),
            AIFieldMap.layers: AIChange(source: .auto, time: nil, before: nil),
        ])
        let json = AIChangesRecord.encode(marks)
        let obj = try #require(try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
        #expect(obj["version"] as? Int == 1)
        #expect((obj["steps"] as? [Any])?.count == 3, "Optimize's two rows share one step")
        let back = try #require(AIChangesRecord.decode(json))
        #expect(back == marks)
        #expect(AIChangesRecord.decode(AIChangesRecord.encode(nil)) == nil)
        #expect(AIChangesRecord.encode(nil) == #"{"steps":[],"version":1}"#)
        // Unreadable records, and rows or sources this version doesn't know, are skipped.
        #expect(AIChangesRecord.decode("{") == nil)
        #expect(AIChangesRecord.decode(#"{"version":1}"#) == nil)
        let odd = #"{"version":1,"steps":[{"source":"robot","rows":[{"panel":"exposure","name":"ev"}]},"#
            + #"{"source":"auto","rows":[{"panel":"exposure","name":"future"},{"panel":"nope","name":"ev"},{"panel":"exposure","name":"ev"}]}]}"#
        #expect(AIChangesRecord.decode(odd) == AIMarks(changes: [Self.ev: AIChange(source: .auto)]))
    }

    @Test func prunedDropsRowsBackAtTheirEarlierValues() {
        var now = EditState()
        now.exposure.ev = 0.5
        let marks = AIMarks(changes: [Self.ev: AIChange(source: .auto, before: EditState()),
                                      Self.vibrance: AIChange(source: .auto, before: EditState())])
        #expect(marks.pruned(against: now)?.fields == [Self.ev])
        #expect(marks.pruned(against: EditState()) == nil)
    }

    /// The changes are kept in the sidecar: a relaunch (a new model on the same store)
    /// lists them again, Undo works, and a cleared set stays cleared (the old report's
    /// rows don't come back).
    @Test func changesSurviveARelaunch() async throws {
        let store = MockSessionStore(counts: [6])
        let m = await Self.model(store)
        let (a, b) = (m.visibleIDs[0], m.visibleIDs[1])
        Self.aiStep(m, .command("brighter"), ids: [a]) { $0.exposure.ev = 0.6 }
        Self.aiStep(m, .auto, ids: [b]) { $0.presence.vibrance = 10 }
        m.clearAIHighlights() // the focused photo, a
        await m.reportSaves?.value
        #expect(try await store.aiChanges(for: a) == AIChangesRecord.encode(nil))
        Self.aiStep(m, .command("brighter"), ids: [a]) { $0.exposure.contrast = 20 }
        await m.reportSaves?.value
        await m.saver.flush()

        let r = await Self.model(store)
        #expect(r.aiMarks.isEmpty)
        _ = await r.loadOptimizeReport(for: a)
        _ = await r.loadOptimizeReport(for: b)
        #expect(r.aiMarks[a]?.fields == [Self.contrast])
        #expect(r.aiMarks[a]?.changes[Self.contrast]?.source == .command("brighter"))
        #expect(r.aiMarks[b]?.fields == [Self.vibrance])
        r.focus(a, extend: false)
        #expect(r.aiNote(Self.contrast) == "AI changed it to +20 (was 0) — ⌘K: “brighter”")
        r.undoAIChanges([Self.contrast])
        #expect(r.edit(for: a).exposure.contrast == 0 && r.edit(for: a).exposure.ev == 0.6)
        await r.reportSaves?.value
        #expect(try await store.aiChanges(for: a) == AIChangesRecord.encode(nil))
        // Loaded once: a second load doesn't replace this run's set.
        Self.aiStep(r, .auto, ids: [b]) { $0.hdr.shadows = 5 }
        _ = await r.loadOptimizeReport(for: b)
        #expect(r.aiMarks[b]?.fields == [Self.vibrance, AIField(.hdr, "shadows")])
    }

    /// Undo All reads photos not shown yet this run before undoing their changes.
    @Test func undoAllReadsUnloadedPhotos() async throws {
        let store = MockSessionStore(counts: [6])
        let m = await Self.model(store)
        let (a, b) = (m.visibleIDs[0], m.visibleIDs[1])
        Self.aiStep(m, ids: [a, b]) { $0.exposure.ev = 0.6 }
        await m.reportSaves?.value
        await m.saver.flush()
        let r = await Self.model(store)
        r.focus(a, extend: false)
        r.selection = [a, b]
        r.editAllSelected = true
        #expect(!r.canUndoAllAIChanges, "not read yet")
        await r.undoAllAIChanges()
        #expect(r.edit(for: a).exposure.ev == 0 && r.edit(for: b).exposure.ev == 0)
    }

    /// A stored record whose rows the edit no longer has (changed outside this app) is pruned.
    @Test func aStaleRecordIsPrunedOnLoad() async throws {
        let store = MockSessionStore(counts: [6])
        let m = await Self.model(store)
        let a = m.visibleIDs[0]
        try await store.saveAIChanges(AIChangesRecord.encode(AIMarks(changes: [
            Self.ev: AIChange(source: .auto, before: EditState()),
        ])), for: [a])
        _ = await m.loadOptimizeReport(for: a)
        #expect(m.aiMarks[a] == nil, "exposure is at its earlier value")
    }
}
