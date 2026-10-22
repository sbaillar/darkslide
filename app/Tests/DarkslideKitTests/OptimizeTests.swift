import CoreGraphics
import DarkslideAI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

// MARK: - Fixtures

/// A Rust-shaped report (what `optimize_analyze` returns), for the real `optimize_apply`.
enum ReportJSON {
    struct C {
        var id: Int
        var field: String
        var new: String
        var old = #"{"kind":"number","value":0.0}"#
        var label = ""
        var from = "0"
        var to = "+1"
        var defaultOn = true
        var explanation = ""
    }

    static func make(_ changes: [C], kept: [(field: String, why: String, explanation: String)] = [],
                     summary: String = "AI Optimize suggests changes.") -> String {
        let cs = changes.map { c in
            """
            {"id":\(c.id),"field":"\(c.field)","label":"\(c.label.isEmpty ? c.field : c.label)","old":\(c.old),"new":\(c.new),\
            "from":"\(c.from)","to":"\(c.to)","reason":{"rule":"tilted","horizon_deg":1.0},"confidence":0.7,\
            "default_on":\(c.defaultOn),"applied":false,"explanation":"\(c.explanation)"}
            """
        }
        let ks = kept.map { k in
            #"{"field":"\#(k.field)","label":"\#(k.field)","value":"x","why":{"why":"\#(k.why)","clipped_pct":null},"explanation":"\#(k.explanation)"}"#
        }
        return #"{"version":1,"changes":[\#(cs.joined(separator: ","))],"kept":[\#(ks.joined(separator: ","))],"summary":"\#(summary)","elapsed_ms":42.0}"#
    }

    /// Exposure +0.45, vibrance +12, a lens suggestion (unchecked) and a straighten.
    static var typical: String {
        make([
            C(id: 0, field: "exposure", new: #"{"kind":"number","value":0.45}"#, label: "Exposure", from: "+0.00 EV",
              to: "+0.45 EV", explanation: "Exposure +0.45 EV — the image was about half a stop dark (midtones at 11% vs the 18% target)."),
            C(id: 1, field: "vibrance", new: #"{"kind":"number","value":12.0}"#, label: "Vibrance", from: "0", to: "+12",
              explanation: "Vibrance +12 — colors were muted (average saturation 20% vs about 28%)."),
            C(id: 2, field: "lens_correction", new: #"{"kind":"toggle","on":true}"#, old: #"{"kind":"toggle","on":false}"#,
              label: "Lens corrections", from: "Off", to: "On", defaultOn: false,
              explanation: "Lens corrections On — lens corrections are off. You turned them off, so this is unchecked."),
            C(id: 3, field: "straighten", new: #"{"kind":"straighten","angle_deg":-1.8}"#, label: "Straighten",
              from: "0.0°", to: "−1.8°", explanation: "Straighten −1.8° — the horizon was tilted 1.8° (rising to the right)."),
        ], kept: [("contrast", "user_set", "Contrast stays at +20: you set it.")],
        summary: "AI Optimize suggests 4 changes: it brightens the image by 0.45 EV, boosts muted colors, suggests turning lens corrections back on and levels a 1.8° tilt. It left contrast alone because you set it.")
    }
}

/// Answers with canned reports (per photo) and applies them with the real Rust code.
final class StubOptimizer: PhotoOptimizing, @unchecked Sendable {
    private let lock = NSLock()
    var reports: [PhotoID: String] = [:]
    var failing: Set<PhotoID> = []
    var json = ReportJSON.typical
    private var _seen: [(PhotoID, OptimizeSignals, OptimizeReport?)] = []
    var seen: [(PhotoID, OptimizeSignals, OptimizeReport?)] { lock.withLock { _seen } }

    func analyze(_ photo: Photo, edit: EditState, signals: OptimizeSignals, prior: OptimizeReport?,
                 masks: DevelopRenderer.MaskProvider?) async throws -> OptimizeReport {
        lock.withLock { _seen.append((photo.id, signals, prior)) }
        if failing.contains(photo.id) { throw OptimizeError.unavailable("no file") }
        return try OptimizeReport(json: reports[photo.id] ?? json)
    }

    func apply(_ report: OptimizeReport, selected: Set<Int>, to edit: EditState) throws -> (edit: EditState, report: OptimizeReport) {
        try RustPhotoOptimizer().apply(report, selected: selected, to: edit)
    }
}

@MainActor
func optimizeModel(_ optimizer: StubOptimizer = StubOptimizer(), store: MockSessionStore = MockSessionStore(counts: [6])) async -> AppModel {
    let m = makeModel(store: store)
    m.toastDuration = .seconds(60)
    m.optimizer = optimizer
    m.optimizeSignals = { _ in
        var s = OptimizeSignals()
        s.horizonDegrees = 1.8
        return s
    }
    await m.start()
    return m
}

// MARK: - Report

@Suite struct OptimizeReportTests {
    @Test func decodesRustsReport() throws {
        let r = try OptimizeReport(json: ReportJSON.typical)
        #expect(r.changes.count == 4 && r.elapsedMs == 42)
        #expect(r.changes[0].field == .exposure && r.changes[0].to == "+0.45 EV")
        #expect(r.defaultSelection == [0, 1, 3], "the lens suggestion starts unchecked")
        #expect(r.kept.first?.isUserSet == true && r.kept.first?.field == .contrast)
        #expect(r.compactSummary.hasPrefix("Exposure +0.45 EV · Vibrance +12"))
        #expect(r.appliedChanges.isEmpty)
        let empty = try OptimizeReport(json: "{}")
        #expect(empty.changes.isEmpty && empty.compactSummary == "No changes needed")
        // A field a newer core added still decodes.
        let newer = try OptimizeReport(json: ReportJSON.make([.init(id: 0, field: "grain", new: #"{"kind":"number","value":1}"#)]))
        #expect(newer.changes[0].field == .other)
        #expect(throws: (any Error).self) { try OptimizeReport(json: "nope") }
        for f in OptimizeField.allCases { #expect(!f.symbol.isEmpty) }
        // Kept notes encode back to Rust's shape.
        let data = try JSONEncoder().encode(r.kept[0])
        let back = try JSONDecoder().decode(OptimizeKept.self, from: data)
        #expect(back == r.kept[0])
    }

    @Test func realApplySetsTheChosenFieldsAndMarksThem() throws {
        let r = try OptimizeReport(json: ReportJSON.typical)
        var edit = EditState()
        edit.exposure.contrast = 20
        let (out, marked) = try RustPhotoOptimizer().apply(r, selected: [0, 1, 3], to: edit)
        #expect(out.exposure.ev == 0.45 && out.presence.vibrance == 12 && abs(out.crop.angle + 1.8) < 1e-6)
        #expect(out.exposure.contrast == 20, "untouched fields stay")
        #expect(out.lens.enabled == edit.lens.enabled)
        #expect(marked.appliedChanges.map(\.id) == [0, 1, 3])
        // An empty report changes nothing.
        let none = try RustPhotoOptimizer().apply(try OptimizeReport(json: "{}"), selected: [0], to: edit)
        #expect(none.edit == edit && none.report.changes.isEmpty)
    }
}

// MARK: - Signals

@Suite struct OptimizeSignalsTests {
    @Test func labelsBecomeSceneTags() {
        #expect(OptimizeSignals.tags(labels: ["mountain", "sky"], faces: 0, faceArea: 0) == [.landscape])
        #expect(OptimizeSignals.tags(labels: ["snow_mountain"], faces: 0, faceArea: 0).contains(.snow))
        #expect(OptimizeSignals.tags(labels: ["sunset_sunrise", "beach"], faces: 0, faceArea: 0) == [.sunset, .beach])
        #expect(OptimizeSignals.tags(labels: ["night_sky"], faces: 0, faceArea: 0) == [.night])
        #expect(OptimizeSignals.tags(labels: [], faces: 1, faceArea: 0.05) == [.portrait])
        #expect(OptimizeSignals.tags(labels: ["people"], faces: 0, faceArea: 0).isEmpty, "no face, no portrait")
        #expect(OptimizeSignals.tags(labels: ["Mountainside"], faces: 0, faceArea: 0).isEmpty, "whole words only")
    }

    @Test func visionAnalysisMapsToTheEFrame() throws {
        let face = FaceInfo(boundingBox: CGRect(x: 0.1, y: 0.1, width: 0.2, height: 0.25), captureQuality: 0.8,
                            leftEyeAspect: nil, rightEyeAspect: nil)
        let image = try #require(solidImage())
        let stats = try #require(ImageStatistics(image: image))
        let a = ImageAnalysis(statistics: stats, faces: [face], aesthetics: nil, featurePrint: nil,
                              labels: [ClassificationLabel(identifier: "beach", confidence: 0.9)], horizonDegrees: 2.0,
                              salientRegions: [CGRect(x: 0.2, y: 0.6, width: 0.3, height: 0.3),
                                               CGRect(x: 0, y: 0, width: 0.1, height: 0.1)])
        let sky = VisionMask(width: 2, height: 2, bytes: [255, 255, 0, 0])
        let s = OptimizeSignals.from(a, sky: sky, aiDenoiseAvailable: true)
        #expect(s.faces == 1 && abs(s.faceArea - 0.05) < 1e-9)
        #expect(s.horizonDegrees == 2.0 && s.skyFraction == 0.5 && s.aiDenoiseAvailable)
        let subject = try #require(s.subject)
        #expect(abs(subject.minY - 0.1) < 1e-9 && abs(subject.width - 0.3) < 1e-9, "the largest region, y flipped")
        #expect(s.tags == [.beach, .portrait])
        let ffi = s.ffi
        #expect(ffi.skyWidth == 2 && ffi.skyMask.count == 4 && ffi.tags == ["beach", "portrait"] && ffi.faces == 1)
        let none = OptimizeSignals.from(nil, sky: VisionMask(width: 1, height: 1, bytes: [0]), aiDenoiseAvailable: false)
        #expect(none.skyMask == nil && none.faces == 0 && none.ffi.subject == nil)
    }

    func solidImage() -> CGImage? {
        let ctx = CGContext(data: nil, width: 8, height: 8, bitsPerComponent: 8, bytesPerRow: 32,
                            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        ctx?.setFillColor(gray: 0.5, alpha: 1)
        ctx?.fill(CGRect(x: 0, y: 0, width: 8, height: 8))
        return ctx?.makeImage()
    }
}

// MARK: - Panel

@MainActor
@Suite struct OptimizePanelTests {
    @Test func analyzesPreviewsTogglesAndAppliesAsOneUndoStep() async throws {
        let stub = StubOptimizer()
        let store = MockSessionStore(counts: [6])
        let m = await optimizeModel(stub, store: store)
        let id = try #require(m.focusedID)
        m.updateEdit(label: "Contrast") { $0.exposure.contrast = 20 }
        m.endEditGesture()
        let before = m.edit(for: id)
        let depth = m.history.undoStack.count
        #expect(CommandCatalog.isEnabled(.aiOptimize, model: m))
        #expect(CommandCatalog.title(.aiOptimize, model: m) == "AI Optimize…")
        CommandCatalog.perform(.aiOptimize, model: m)
        let panel = try #require(m.optimizePanel)
        #expect(!m.canOptimize, "one panel at a time")
        #expect(panel.progressText == "Analyzing…" || panel.report != nil)
        await panel.wait()
        let r = try #require(panel.report)
        #expect(panel.selected == [0, 1, 3] && panel.applyTitle == "Apply 3 Changes" && panel.canApply)
        #expect(panel.summary == r.summary, "the template: values and words both come from the rules")
        #expect(stub.seen.first?.1.horizonDegrees == 1.8, "the scene signals reach the analysis")
        // Live preview: the viewer renders the checked changes.
        #expect(m.viewer.previewEdit?.exposure.ev == 0.45)
        panel.toggle(0)
        #expect(m.viewer.previewEdit?.exposure.ev == 0 && panel.applyTitle == "Apply 2 Changes")
        panel.toggle(2)
        #expect(m.viewer.previewEdit?.lens.enabled == !before.lens.enabled || m.viewer.previewEdit?.lens.enabled == true)
        panel.toggle(2)
        panel.toggle(99) // not a change: ignored
        panel.toggle(0)
        panel.showBefore = true
        #expect(m.viewer.previewEdit == before)
        panel.showBefore = false
        #expect(m.viewer.previewEdit?.exposure.ev == 0.45)
        #expect(m.edit(for: id) == before, "nothing is stored before Apply")
        panel.apply()
        #expect(m.optimizePanel == nil && panel.isFinished && m.viewer.previewEdit == nil)
        let after = m.edit(for: id)
        #expect(after.exposure.ev == 0.45 && after.presence.vibrance == 12 && abs(after.crop.angle + 1.8) < 1e-6)
        #expect(after.exposure.contrast == 20 && after.lens == before.lens, "the user's contrast and lens stay")
        #expect(m.history.undoStack.count == depth + 1 && m.history.undoLabel == "AI Optimize")
        #expect(m.toast?.message == "AI Optimize: 3 changes applied")
        // The report is kept (this run and in the store) for "Why these settings?".
        let kept = try #require(m.optimizeReports[id])
        #expect(kept.appliedChanges.map(\.id) == [0, 1, 3])
        var stored: OptimizeReport?
        for _ in 0..<2000 where stored == nil {
            stored = try await store.optimizeReport(for: id)
            if stored == nil { try await Task.sleep(for: .milliseconds(2)) }
        }
        #expect(stored?.appliedChanges.count == 3)
        m.showWhy()
        #expect(m.whyReport == kept && m.whyChanges.map(\.id) == [0, 1, 3])
        #expect(OptimizeWhy.intro(kept, inEffect: 3).contains("these 3 values"))
        // A later slider move takes one value out of "Why"; undo takes all of them.
        m.updateEdit(label: "Vibrance") { $0.presence.vibrance = 30 }
        #expect(m.whyChanges.map(\.id) == [0, 3])
        #expect(OptimizeWhy.intro(kept, inEffect: 2).contains("1 more were changed since"))
        m.undo()
        // One undo step takes the optimize back.
        m.undo()
        #expect(m.edit(for: id) == before)
        #expect(m.whyChanges.isEmpty && OptimizeWhy.intro(kept, inEffect: 0).contains("all been changed since"))
        m.whyReportPhoto = nil
        // The undo took the report back too (NEW-1): a second run has no prior. Once redone,
        // a run hands the stored report to the analysis as `prior`.
        m.beginOptimize()
        await m.optimizePanel?.wait()
        #expect(stub.seen.last?.2 == nil)
        m.endOptimize()
        m.redo()
        m.beginOptimize()
        await m.optimizePanel?.wait()
        #expect(stub.seen.last?.2?.appliedChanges.count == 3)
        m.endOptimize()
    }

    @Test func cancelRevertsThePreviewAndChangesNothing() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        let before = m.edit(for: id)
        m.beginOptimize()
        let panel = try #require(m.optimizePanel)
        await panel.wait()
        #expect(m.viewer.previewEdit != nil)
        m.endOptimize()
        #expect(m.optimizePanel == nil && m.viewer.previewEdit == nil && m.edit(for: id) == before)
        panel.apply() // closed: no effect
        #expect(m.edit(for: id) == before && m.optimizeReports[id] == nil)
    }

    /// The panel always has a way out, whatever the toolbar shows: Esc and ⌥⌘A close it,
    /// and so does moving to another photo (Apply never lands on a photo not on screen).
    @Test func thePanelClosesOnEscCommandAndFocusChange() async throws {
        let m = await optimizeModel()
        let first = try #require(m.focusedID)
        m.beginOptimize()
        await m.optimizePanel?.wait()
        #expect(CommandCatalog.title(.aiOptimize, model: m) == "Close AI Optimize")
        #expect(CommandCatalog.isEnabled(.aiOptimize, model: m))
        CommandCatalog.perform(.aiOptimize, model: m)
        #expect(m.optimizePanel == nil && m.viewer.previewEdit == nil && m.canOptimize)
        m.beginOptimize()
        await m.optimizePanel?.wait()
        m.perform(.escape)
        #expect(m.optimizePanel == nil && m.viewer.previewEdit == nil)
        m.beginOptimize()
        let panel = try #require(m.optimizePanel)
        await panel.wait()
        m.focus(m.visibleIDs[1], extend: false)
        #expect(m.optimizePanel == nil && panel.isFinished && m.viewer.previewEdit == nil)
        panel.apply()
        #expect(m.edit(for: first) == EditState() && m.edit(for: m.visibleIDs[1]) == EditState())
    }

    /// NEW-2: an edit, undo or redo while the panel is open closes it (its preview would
    /// be stale and Apply would overwrite the edit); the edit stays.
    @Test func anEditWhileOpenClosesThePanel() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        m.beginOptimize()
        let panel = try #require(m.optimizePanel)
        await panel.wait()
        m.updateEdit(label: "Levels") { $0.levels.gamma = 1.4 }
        #expect(m.optimizePanel == nil && panel.isFinished && m.viewer.previewEdit == nil)
        panel.apply()
        #expect(m.edit(for: id).levels.gamma == 1.4 && m.edit(for: id).exposure.ev == 0)
        m.beginOptimize()
        await m.optimizePanel?.wait()
        m.undo()
        #expect(m.optimizePanel == nil && m.edit(for: id) == EditState())
    }

    /// The merge keeps what changed meanwhile, field by field, and ignores JSON round-off.
    @Test func mergeIsPerFieldWithATolerance() {
        var base = EditState()
        base.levels.gamma = 1.0
        var result = base
        result.levels.inBlack = 0.05
        result.noise.luminance = 40
        result.lens.enabled = false
        result.exposure.contrast = base.exposure.contrast + 1e-12 // round-off only
        var current = base
        current.levels.gamma = 1.4
        current.noise.color = 60
        current.lens.lightFalloff = 0.8
        current.exposure.contrast = 20
        let out = AIEditMerge.apply(result: result, base: base, current: current)
        #expect(out.levels.inBlack == 0.05 && out.levels.gamma == 1.4)
        #expect(out.noise.luminance == 40 && out.noise.color == 60)
        #expect(!out.lens.enabled && out.lens.lightFalloff == 0.8)
        #expect(out.exposure.contrast == 20, "round-off doesn't count as a change")
    }

    /// NEW-1: the report belongs to the undo step. Apply → Undo → run again proposes the
    /// same changes (nothing reads as the user's), and Redo brings the report back.
    @Test func undoRestoresTheReportSoARerunProposesTheSame() async throws {
        let stub = StubOptimizer()
        let store = MockSessionStore(counts: [6])
        let m = await optimizeModel(stub, store: store)
        let id = try #require(m.focusedID)
        m.beginOptimize()
        await m.optimizePanel?.wait()
        m.optimizePanel?.apply()
        let applied = try #require(m.optimizeReports[id])
        m.undo()
        #expect(m.optimizeReports[id] == nil && m.edit(for: id) == EditState())
        await m.reportSaves?.value
        #expect(try await store.optimizeReport(for: id) == nil, "the sidecar's report is cleared too")
        m.beginOptimize()
        let panel = try #require(m.optimizePanel)
        await panel.wait()
        #expect(stub.seen.last?.2 == nil, "no prior report: the undone values aren't remembered")
        #expect(panel.selected == [0, 1, 3])
        m.endOptimize()
        m.redo()
        #expect(m.optimizeReports[id] == applied && m.edit(for: id).exposure.ev == 0.45)
        await m.reportSaves?.value
        #expect(try await store.optimizeReport(for: id) == applied)
        // A new variant waits for the write (it copies the sidecar).
        #expect(await m.loadOptimizeReport(for: id) == applied)
    }

    /// NEW-1 follow-up: apply → the user resets a value → another run applied → undo both:
    /// the optimizer's value is back with the report that set it, so the next run is told
    /// the optimizer set it (it isn't taken for the user's).
    @Test func undoingAResetOverAnOptimizerValueKeepsItTheOptimizers() async throws {
        let stub = StubOptimizer()
        let m = await optimizeModel(stub)
        let id = try #require(m.focusedID)
        m.beginOptimize()
        await m.optimizePanel?.wait()
        m.optimizePanel?.apply()
        let r1 = try #require(m.optimizeReports[id])
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 0 }
        stub.json = ReportJSON.make([.init(id: 0, field: "exposure", new: #"{"kind":"number","value":0.3}"#)])
        m.beginOptimize()
        await m.optimizePanel?.wait()
        #expect(stub.seen.last?.2 == r1)
        m.optimizePanel?.apply()
        #expect(m.optimizeReports[id] != r1 && m.edit(for: id).exposure.ev == 0.3)
        m.undo()
        m.undo()
        #expect(m.edit(for: id).exposure.ev == 0.45 && m.optimizeReports[id] == r1)
        m.beginOptimize()
        await m.optimizePanel?.wait()
        #expect(stub.seen.last?.2 == r1, "the prior report says the optimizer set +0.45")
        m.endOptimize()
    }

    /// NEW-3: ⏎ applies and Tab moves into the panel while it is open; Esc closes it,
    /// even in survey mode.
    @Test func keysWhileThePanelIsOpen() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        m.beginOptimize()
        await m.optimizePanel?.wait()
        #expect(!m.handleKey(KeyInput(special: .tab), focus: .none, sheetOpen: false, isMainWindow: true),
                "Tab goes to the panel's controls")
        #expect(m.optimizePanel != nil)
        #expect(m.handleKey(KeyInput(special: .returnKey), focus: .none, sheetOpen: false, isMainWindow: true))
        #expect(m.optimizePanel == nil && m.edit(for: id).exposure.ev == 0.45)
        m.beginOptimize()
        await m.optimizePanel?.wait()
        #expect(m.handleKey(KeyInput(special: .escape), focus: .none, sheetOpen: false, isMainWindow: true))
        #expect(m.optimizePanel == nil)
        // In a text field the keys are the field's.
        m.beginOptimize()
        await m.optimizePanel?.wait()
        _ = m.handleKey(KeyInput(special: .returnKey), focus: .text, sheetOpen: false, isMainWindow: true)
        #expect(m.optimizePanel != nil)
        m.endOptimize()
    }

    @Test func failuresAreShownAndNothingApplies() async throws {
        let stub = StubOptimizer()
        let m = await optimizeModel(stub)
        let id = try #require(m.focusedID)
        stub.failing = [id]
        m.beginOptimize()
        let panel = try #require(m.optimizePanel)
        await panel.wait()
        #expect(panel.items.first?.state == .failed("no file") && !panel.canApply && panel.report == nil)
        #expect(panel.previewEdit == nil)
        panel.apply()
        #expect(m.optimizePanel != nil, "nothing to apply keeps the panel open")
        m.endOptimize()
        // Without an optimizer (demo) the command explains itself.
        m.optimizer = nil
        #expect(!m.canOptimize)
        m.beginOptimize()
        #expect(m.optimizePanel == nil && m.toast?.isError == true)
    }

    @Test func batchAnalyzesEachPhotoAndAppliesAllAsOneStep() async throws {
        let stub = StubOptimizer()
        let store = MockSessionStore(counts: [6])
        let m = await optimizeModel(stub, store: store)
        let ids = Array(m.visibleIDs.prefix(4))
        m.focus(ids[0], extend: false)
        for i in ids.dropFirst() { m.focus(i, extend: true) }
        #expect(m.optimizeTargets.count == 4)
        #expect(CommandCatalog.title(.aiOptimize, model: m) == "AI Optimize 4 Photos…")
        #expect(ToolbarControl.optimize.control(m).label == "AI Optimize 4 photos")
        stub.failing = [ids[3]]
        stub.reports[ids[2]] = ReportJSON.make([], summary: "Already balanced.")
        let before = Dictionary(uniqueKeysWithValues: ids.map { ($0, m.edit(for: $0)) })
        let depth = m.history.undoStack.count
        m.beginOptimize()
        let panel = try #require(m.optimizePanel)
        #expect(panel.isBatch && panel.progressText?.hasPrefix("Analyzing") == true)
        await panel.wait()
        #expect(panel.progressText == nil)
        #expect(panel.items.map(\.report?.changes.count) == [4, 4, 0, nil])
        #expect(panel.applyTitle == "Apply to 2 Photos")
        #expect(m.viewer.previewEdit == nil, "no single preview for a batch")
        panel.setInclude(ids[1], false)
        #expect(panel.applyTitle == "Apply to 1 Photo")
        panel.setInclude(ids[1], true)
        panel.setInclude(PhotoID(-5), false) // unknown: ignored
        panel.apply()
        #expect(m.history.undoStack.count == depth + 1 && m.history.undoLabel == "AI Optimize 2 Photos")
        for id in ids.prefix(2) {
            #expect(m.edit(for: id).exposure.ev == 0.45 && m.edit(for: id).lens == before[id]!.lens, "default-on only")
            #expect(m.optimizeReports[id] != nil)
        }
        for id in ids.suffix(2) { #expect(m.edit(for: id) == before[id]!) }
        #expect(m.toast?.message == "AI Optimize: 6 changes on 2 photos")
        m.undo()
        for id in ids { #expect(m.edit(for: id) == before[id]!) }
    }

    /// The live signals: one Vision pass on the photo's preview; the sky mask only when
    /// the sky model is installed.
    @Test func visionSignalsComeFromThePreview() async throws {
        let m = await optimizeModel()
        let p = try #require(m.focusedPhoto)
        let s = await m.visionOptimizeSignals(p, aiDenoiseAvailable: true, skyModelReady: false)
        #expect(s.aiDenoiseAvailable && s.skyMask == nil && s.skyFraction == 0)
        // Nothing to show "Why" for yet.
        m.showWhy()
        #expect(m.whyReportPhoto == nil && m.whyReport == nil)
        #expect(await m.loadOptimizeReport(for: p.id) == nil)
        let none = try OptimizeReport(json: ReportJSON.typical)
        #expect(OptimizeWhy.intro(none, inEffect: 0).contains("left its settings"))
        let one = try RustPhotoOptimizer().apply(none, selected: [1], to: EditState()).report
        #expect(OptimizeWhy.intro(one, inEffect: 1).contains("this value"))
        #expect(!OptimizeWhy.caveat.isEmpty && !OptimizeWhy.buttonLabel.isEmpty && !OptimizeWhy.buttonHelp.isEmpty)
    }

    @Test func aPreviewOnlyFollowsTheFocusedPhoto() async throws {
        let m = await optimizeModel()
        m.beginOptimize()
        let panel = try #require(m.optimizePanel)
        await panel.wait()
        m.focus(m.visibleIDs[1], extend: false)
        panel.toggle(0)
        #expect(m.viewer.previewEdit == nil, "another photo is on screen")
    }
}

// MARK: - Storage

@Suite struct OptimizeStorageTests {
    @Test func mockAndFolderStoresKeepReports() async throws {
        let r = try OptimizeReport(json: ReportJSON.typical)
        let mock = MockSessionStore(counts: [1])
        try await mock.saveOptimizeReport(r, for: [PhotoID(1)])
        #expect(try await mock.optimizeReport(for: PhotoID(1)) == r)
        try await mock.saveOptimizeReport(nil, for: [PhotoID(1)])
        #expect(try await mock.optimizeReport(for: PhotoID(1)) == nil)
        let folder = FolderSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!),
                                        isSupported: { _ in true }, loader: { _, id in samplePhoto(id.raw) })
        try await folder.saveOptimizeReport(r, for: [PhotoID(2)])
        #expect(try await folder.optimizeReport(for: PhotoID(2)) == r)
    }

    /// The real thing: analyze an A9 frame in Rust, apply, keep the report in the
    /// sidecar, and read it back after a reopen (skips without samples).
    @Test func liveReportRoundTripsThroughTheSidecar() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let base = FileManager.default.temporaryDirectory.appendingPathComponent("ds-store-\(UUID())")
        defer { try? FileManager.default.removeItem(at: base) }
        let recents = RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!)
        let s = RustSessionStore(recents: recents, storeBase: base)
        let info = try await s.add(folder: dir)
        let photo = try #require(try await s.photos(in: info).first)
        var signals = OptimizeSignals()
        signals.horizonDegrees = 2
        let clock = ContinuousClock()
        let t0 = clock.now
        let report = try await RustPhotoOptimizer().analyze(photo, edit: EditState(), signals: signals, prior: nil, masks: nil)
        print("AI Optimize live analysis \(clock.now - t0) (Rust \(report.elapsedMs) ms): \(report.summary)")
        #expect(!report.changes.isEmpty && report.changes.allSatisfy { !$0.explanation.isEmpty })
        #expect(report.changes.contains { $0.field == .straighten })
        let (edit, marked) = try RustPhotoOptimizer().apply(report, selected: report.defaultSelection, to: EditState())
        #expect(edit != EditState())
        try await s.saveOptimizeReport(marked, for: [photo.id])
        try await s.save(edits: [photo.id: edit])
        let s2 = RustSessionStore(recents: recents, storeBase: base)
        let info2 = try await s2.add(folder: dir)
        _ = try await s2.photos(in: info2)
        let back = try #require(try await s2.optimizeReport(for: photo.id))
        #expect(back.changes == marked.changes && back.summary == marked.summary)
        // A second analysis with the stored report as prior proposes nothing new.
        let again = try await RustPhotoOptimizer().analyze(photo, edit: try await s2.edit(for: photo.id), signals: signals,
                                                          prior: back, masks: nil)
        #expect(again.changes.filter(\.defaultOn).isEmpty, "\(again.changes.map(\.explanation))")
        try await s2.saveOptimizeReport(nil, for: [photo.id])
        #expect(try await s2.optimizeReport(for: photo.id) == nil)
        #expect(try await s2.optimizeReport(for: PhotoID(987_654)) == nil)
    }
}
