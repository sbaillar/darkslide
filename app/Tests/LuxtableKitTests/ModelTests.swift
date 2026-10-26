import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

@Suite struct EditHistoryTests {
    func step(_ label: String, key: String? = nil, ev: Double, time: Date = Date()) -> HistoryStep {
        var s = HistoryStep(label: label, coalesceKey: key, time: time)
        var a = EditState()
        a.exposure.ev = ev
        s.editsBefore[PhotoID(1)] = EditState()
        s.editsAfter[PhotoID(1)] = a
        return s
    }

    @Test func undoRedoOrder() {
        var h = EditHistory()
        #expect(!h.canUndo && !h.canRedo && h.undo() == nil && h.redo() == nil)
        h.record(step("a", ev: 1))
        h.record(step("b", ev: 2))
        #expect(h.undoLabel == "b")
        #expect(h.undo()?.label == "b")
        #expect(h.redoLabel == "b")
        #expect(h.redo()?.label == "b")
        _ = h.undo()
        h.record(step("c", ev: 3))
        #expect(!h.canRedo, "a new step clears redo")
    }

    @Test func coalescesPerGestureNotPerTimer() {
        var h = EditHistory()
        let t0 = Date()
        h.record(step("ev", key: "ev", ev: 1, time: t0))
        h.record(step("ev", key: "ev", ev: 2, time: t0.addingTimeInterval(5)))
        #expect(h.undoStack.count == 1, "a slow drag with a pause stays one step")
        #expect(h.undoStack[0].editsAfter[PhotoID(1)]?.exposure.ev == 2)
        #expect(h.undoStack[0].editsBefore[PhotoID(1)] == EditState(), "keeps the oldest before")
        h.endGesture()
        h.record(step("ev", key: "ev", ev: 3, time: t0.addingTimeInterval(5.1)))
        #expect(h.undoStack.count == 2, "a second quick drag is its own step")
        h.record(step("x", key: "other", ev: 4, time: t0.addingTimeInterval(5.2)))
        #expect(h.undoStack.count == 3, "a different control starts a new step")
        h.record(step("plain", ev: 5))
        h.record(step("x", key: "other", ev: 6))
        #expect(h.undoStack.count == 5, "an uncoalesced step closes the gesture")
        _ = h.undo()
        h.record(step("x", key: "other", ev: 7))
        #expect(h.undoStack.count == 5 && h.undoStack.last?.editsAfter[PhotoID(1)]?.exposure.ev == 7, "undo closes the gesture")
    }

    @Test func emptyStepsIgnoredAndLimitEnforced() {
        var h = EditHistory(limit: 3)
        h.record(HistoryStep(label: "noop"))
        #expect(!h.canUndo)
        for i in 0..<5 { h.record(step("s\(i)", ev: Double(i + 1))) }
        #expect(h.undoStack.map(\.label) == ["s2", "s3", "s4"])
    }
}

@Suite struct ClipboardTests {
    func edited() -> EditState {
        var e = EditState()
        e.whiteBalance = { var w = WhiteBalanceEdit(); w.asShot = false; w.temperature = 6600; w.tint = 10; return w }()
        e.exposure.ev = 0.5
        e.exposure.contrast = 10
        e.hdr.shadows = 30
        e.levels.gamma = 1.2
        e.curve.points[.rgb] = [CurvePoint(0.25, 0.3)]
        e.colorBalance.shadows = WheelValue(hue: 200, saturation: 0.3, lightness: 0)
        e.colorEditor.bands[2].saturation = 20
        e.detail.clarity = 15
        e.noise.luminance = 40
        e.vignette.amountEV = -0.5
        e.lens.distortion = 0.5
        e.crop.aspect = .r4x5
        e.layers = [LayerEdit(id: 0, name: "Sky"), LayerEdit(id: 1, name: "Face")]
        return e
    }

    @Test func absolutePasteCopiesSelectedGroupsOnly() {
        let clip = AdjustmentClipboard(source: edited())
        var target = EditState()
        target.hdr.shadows = -10
        let out = clip.apply(to: target, options: PasteOptions(groups: [.exposure, .crop]))
        #expect(out.exposure == edited().exposure)
        #expect(out.crop.aspect == .r4x5)
        #expect(out.hdr.shadows == -10)
    }

    @Test func pasteAllCopiesEveryGroup() {
        let src = edited()
        let out = AdjustmentClipboard(source: src).apply(to: EditState(), options: PasteOptions())
        #expect(out.whiteBalance == src.whiteBalance && out.levels == src.levels && out.curve == src.curve)
        #expect(out.colorBalance == src.colorBalance && out.colorEditor == src.colorEditor)
        #expect(out.detail == src.detail && out.noise == src.noise && out.vignette == src.vignette && out.lens == src.lens)
        #expect(out.layers.map(\.name) == ["Sky", "Face"])
        for g in AdjustGroup.allCases where g != .layers { #expect(!g.differs(out, src), "\(g)") }
    }

    @Test func relativePasteAddsDeltasAndClamps() {
        let clip = AdjustmentClipboard(source: edited())
        var target = EditState()
        target.exposure.ev = 4.8
        target.exposure.contrast = 95
        target.hdr.shadows = 10
        target.whiteBalance.asShot = false
        target.whiteBalance.temperature = 5000
        target.whiteBalance.tint = 145
        // [tl8] Relative WB is relative to the source's own as-shot (lt-core `relative_wb`).
        let out = clip.apply(to: target, options: PasteOptions(groups: [.exposure, .hdr, .whiteBalance],
                                                               relative: [.exposure, .hdr, .whiteBalance, .levels]),
                             sourceAsShot: WhiteBalanceValue(kelvin: 5500, tint: 0))
        #expect(out.exposure.ev == 5, "clamped at +5 EV")
        #expect(out.exposure.contrast == 100)
        #expect(out.hdr.shadows == 40)
        #expect(abs(out.whiteBalance.temperature - 5000 * 6600 / 5500) < 1e-6, "Kelvin transfers as a ratio")
        #expect(out.whiteBalance.tint == 150)
        // Without the as-shot context there is no baseline: the source's value, absolutely.
        let noContext = clip.apply(to: target, options: PasteOptions(groups: [.whiteBalance], relative: [.whiteBalance]))
        #expect(noContext.whiteBalance == edited().whiteBalance)
    }

    @Test func relativeWBFallsBackToAbsoluteForAsShot() {
        let out = AdjustmentClipboard(source: edited()).apply(
            to: EditState(), options: PasteOptions(groups: [.whiteBalance], relative: [.whiteBalance]))
        #expect(out.whiteBalance == edited().whiteBalance)
    }

    @Test func layerPasteAppendsWithFreshIDsAndFilters() {
        var target = EditState()
        target.layers = [LayerEdit(id: 7, name: "Mine")]
        let out = AdjustmentClipboard(source: edited()).apply(to: target, options: PasteOptions(groups: [.layers], layerIDs: [1]))
        #expect(out.layers.map(\.name) == ["Mine", "Face"])
        #expect(out.layers.map(\.id) == [7, 8])
    }

    @Test func groupMetadata() {
        #expect(AdjustGroup.allCases.count == 17)
        #expect(AdjustGroup.allCases.filter(\.supportsRelative) == [.whiteBalance, .exposure, .presence, .hdr])
        #expect(Set(AdjustGroup.allCases.map(\.title)).count == 17)
        #expect(AdjustGroup.allCases.allSatisfy { !$0.symbol.isEmpty && $0.id == $0.rawValue })
        #expect(AdjustGroup.changed(in: EditState()).isEmpty)
        var all = edited()
        all.presence.dehaze = 20
        all.spots = [SpotEdit()]
        all.effects.glow.amount = 10
        all.profile = .vivid
        #expect(AdjustGroup.changed(in: all) == Set(AdjustGroup.allCases))
        #expect(EditState().isIdentity && !edited().isIdentity)
    }

    @Test func curveAndAspectHelpers() {
        var c = CurveEdit()
        #expect(c.points(for: .luma).isEmpty)
        c.points[.luma] = [CurvePoint(0.5, 0.6)]
        #expect(c.points(for: .luma).count == 1)
        #expect(CropAspect.free.ratio == nil && CropAspect.original.ratio == nil)
        #expect(CropAspect.r4x5.ratio! == (5, 4) && CropAspect.r1x1.ratio! == (1, 1))
        #expect(CropAspect.r3x2.ratio! == (3, 2) && CropAspect.r16x9.ratio! == (16, 9))
        #expect(ColorEditorEdit.bandNames.count == ColorEditorEdit.bandHues.count)
    }
}

@Suite struct PhotoModelTests {
    @Test func filterMatching() {
        var p = samplePhoto()
        p.rating = 3
        p.colorTag = .red
        p.pick = .picked
        #expect(PhotoFilter().matches(p) && !PhotoFilter().isActive)
        #expect(PhotoFilter(minRating: 3).matches(p) && !PhotoFilter(minRating: 4).matches(p))
        #expect(PhotoFilter(colorTag: .red).matches(p) && !PhotoFilter(colorTag: .blue).matches(p))
        #expect(PhotoFilter(pick: .picked).matches(p) && PhotoFilter(pick: .unrejected).matches(p))
        #expect(!PhotoFilter(pick: .rejected).matches(p))
        p.pick = .rejected
        #expect(!PhotoFilter(pick: .unrejected).matches(p) && !PhotoFilter(pick: .picked).matches(p))
        #expect(PhotoFilter(text: "dsc01").matches(p) && !PhotoFilter(text: "IMG").matches(p))
    }

    @Test func metaAndIDs() {
        #expect(samplePhoto().meta.aspectRatio == 1.5 && !samplePhoto().meta.isPortrait)
        #expect(samplePhoto(portrait: true).meta.isPortrait)
        var m = samplePhoto().meta
        m.pixelHeight = 0
        #expect(m.aspectRatio == 1.5)
        #expect(PhotoID(1) < PhotoID(2) && PhotoID(3).description == "#3")
    }

    @Test func toolTabsListPanelsWithTitles() {
        for tab in ToolTab.allCases {
            #expect(!tab.title.isEmpty && !tab.symbol.isEmpty && !tab.panels.isEmpty && tab.id == tab.rawValue)
        }
        let all = Set(ToolTab.allCases.flatMap(\.panels))
        #expect(all == Set(ToolPanelID.allCases), "every panel lives in exactly one tab")
        #expect(ToolPanelID.allCases.allSatisfy { !$0.title.isEmpty && $0.id == $0.rawValue })
        let groups = Set(ToolPanelID.allCases.compactMap(\.group))
        // The camera profile is picked at the top of the White Balance panel.
        #expect(groups.union([.profile]) == Set(AdjustGroup.allCases), "every group is editable from some panel")
    }

    @Test func activeSheetIDs() {
        #expect(ActiveSheet.importCard(nil).id == "import")
        #expect(ActiveSheet.onboarding.id == "onboarding")
    }
}

@MainActor
@Suite struct AppSettingsTests {
    @Test func defaultsAndPersistence() {
        let suite = testSuiteName()
        let d = UserDefaults(suiteName: suite)!
        let s = AppSettings(defaults: d)
        #expect(!s.onboardingComplete)
        #expect(s.archiveTarget.isEmpty, "L5: suggested by onboarding, not defaulted")
        #expect(s.sessionsFolder.lastPathComponent == "Luxtable")
        #expect(s.appearance == .dark && s.autoOpenImport)
        #expect(s.copyMode == .edited && s.copyGroups == AdjustGroup.copiedByDefault && s.copyRelativeGroups.isEmpty)
        #expect(!s.copyGroups.contains(.crop) && !s.copyGroups.contains(.spots) && !s.copyGroups.contains(.layers))
        #expect(s.sessionNameTemplate == "{date} {text}" && s.renameTemplate == "{original}")

        s.onboardingComplete = true
        s.sessionsFolder = URL(fileURLWithPath: "/tmp/ds")
        s.archiveTarget = "/Volumes/x"
        s.amazonPhotosFolder = URL(fileURLWithPath: "/tmp/amz")
        s.appearance = .light
        s.autoOpenImport = false
        s.copyGroups = [.exposure, .crop]
        s.copyRelativeGroups = [.exposure]
        s.sessionNameTemplate = "{text}"
        s.renameTemplate = "{seq}"

        let r = AppSettings(defaults: d)
        #expect(r.onboardingComplete && r.sessionsFolder.path == "/tmp/ds" && r.archiveTarget == "/Volumes/x")
        #expect(r.amazonPhotosFolder?.path == "/tmp/amz" && r.appearance == .light && !r.autoOpenImport)
        #expect(r.copyGroups == [.exposure, .crop] && r.copyRelativeGroups == [.exposure])
        #expect(r.sessionNameTemplate == "{text}" && r.renameTemplate == "{seq}")
        r.copyGroups = []
        #expect(AppSettings(defaults: d).copyGroups.isEmpty, "nothing ticked is a choice too")
        d.removePersistentDomain(forName: suite)
    }
}

@Suite struct FormatterTests {
    @Test func exposureValues() {
        #expect(Formatters.shutter(1.0 / 250) == "1/250")
        #expect(Formatters.shutter(1.0 / 4000) == "1/4000")
        #expect(Formatters.shutter(0.5) == "0.5\"")
        #expect(Formatters.shutter(2) == "2\"")
        #expect(Formatters.shutter(0) == "–" && Formatters.shutter(.nan) == "–")
        #expect(Formatters.aperture(2.8) == "f/2.8" && Formatters.aperture(8) == "f/8" && Formatters.aperture(0) == "–")
        #expect(Formatters.focal(84.6) == "85 mm")
        #expect(Formatters.kelvin(5500) == "5,500 K")
    }

    @Test func signedValues() {
        #expect(Formatters.signed(0.3) == "+0.30")
        #expect(Formatters.signed(-1) == "-1.00")
        #expect(Formatters.signed(0) == "0.00")
        #expect(Formatters.signed(-0.001) == "0.00", "no negative zero")
        #expect(Formatters.signed(12, decimals: 0) == "+12")
    }

    @Test func transferValues() {
        #expect(Formatters.bytes(48_500_000) == "48.5 MB")
        #expect(Formatters.throughput(bytesPerSecond: 285_000_000) == "285 MB/s")
        #expect(Formatters.throughput(bytesPerSecond: 0) == "–")
        #expect(Formatters.duration(45) == "45 s")
        #expect(Formatters.duration(125) == "2 min 05 s")
        #expect(Formatters.duration(3780) == "1 h 03 min")
        #expect(Formatters.duration(-1) == "–")
    }

    @Test func relativeDates() {
        let now = Date()
        #expect(Formatters.relative(nil) == "never")
        #expect(Formatters.relative(now.addingTimeInterval(-10), now: now) == "just now")
        #expect(Formatters.relative(now.addingTimeInterval(-600), now: now) == "10 min ago")
        #expect(Formatters.relative(now.addingTimeInterval(-7200), now: now) == "2 h ago")
        #expect(Formatters.relative(now.addingTimeInterval(-3 * 86_400), now: now) == "3 d ago")
    }
}

@Suite struct HistogramTests {
    @Test func countsChannelsLumaAndClipping() {
        // 4 pixels: black, white, pure red, mid gray.
        let px: [UInt8] = [0, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 255, 128, 128, 128, 255]
        let h = Histogram(frame: RenderedFrame(width: 2, height: 2, rgba: Data(px)))
        #expect(h.pixelCount == 4)
        #expect(h.red[0] == 1 && h.red[255] == 2 && h.red[128] == 1)
        #expect(h.green[0] == 2 && h.blue[0] == 2)
        #expect(h.luma[0] == 1 && h.luma[255] == 1 && h.luma[128] == 1 && h.luma[53] == 1)
        #expect(h.shadowClipped == 2, "black and pure red have a channel at 0")
        #expect(h.highlightClipped == 2)
        #expect(h.highlightClipFraction == 0.5 && h.shadowClipFraction == 0.5)
        #expect(h.bins(.luma) == h.luma && h.bins(.red) == h.red && h.bins(.green) == h.green && h.bins(.blue) == h.blue)
        let n = h.normalized(.red)
        #expect(n.count == 256 && n.max() == 1 && n[0] > 0 && n[0] < 1)
    }

    @Test func paddedRowsAreBinnedAndPaddingIgnored() {
        // 2x2 image, rows padded to 16 bytes with 0xFF garbage in the padding.
        var bytes = [UInt8](repeating: 0xFF, count: 32)
        for (y, x) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
            let i = y * 16 + x * 4
            bytes[i] = 10; bytes[i + 1] = 20; bytes[i + 2] = 30; bytes[i + 3] = 255
        }
        let f = RenderedFrame(width: 2, height: 2, format: .rgba8Srgb, bytesPerRow: 16, backing: .data(Data(bytes)))
        let h = Histogram(frame: f)
        #expect(h.pixelCount == 4 && h.red[10] == 4 && h.red[255] == 0, "padding must not be counted")
        let float = RenderedFrame(width: 1, height: 1, format: .rgba16fLinearP3, bytesPerRow: 8, backing: .data(Data(count: 8)))
        #expect(Histogram(frame: float).pixelCount == 0)
    }

    @Test func emptyHistogram() {
        let h = Histogram()
        #expect(h.normalized(.luma).allSatisfy { $0 == 0 })
        #expect(h.highlightClipFraction == 0 && h.shadowClipFraction == 0)
        let short = Histogram(rgba: Data([1, 2, 3]), pixelCount: 10)
        #expect(short.pixelCount == 0)
    }
}
