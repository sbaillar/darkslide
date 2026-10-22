import DarkslideAI
import Foundation
import Testing
@testable import DarkslideKit

/// Horizons per photo (nil = none found), for ⌘K on a selection.
private struct PerPhotoHorizon: HorizonDetecting {
    var degrees: [PhotoID: Double]
    func horizon(of photo: Photo, images: any ImageProvider) async throws -> HorizonReading? {
        degrees[photo.id].map { HorizonReading(degrees: $0, confidence: 0.9) }
    }
}

/// Final-journeys round 2: N6 (Copy/Apply defaults), N7 (⌘K scope and undo), N8 (corrupt
/// presets), N12 (WB first), N13 (stale render error) and the Auto toast.
@MainActor
@Suite struct EditPolishTests {
    // MARK: N6

    @Test func copyNeverPreselectsSpotsCropOrLayers() async throws {
        let m = makeModel()
        await m.start()
        let ids = Array(m.visibleIDs.prefix(3))
        m.focus(ids[0], extend: false)
        m.updateEdit(label: "source") {
            $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 6000, tint: 3))
            $0.spots = [SpotEdit()]
            $0.crop.angle = 2
            $0.layers = [LayerEdit(id: 0, name: "Sky")]
        }
        // A target with its own crop and straighten (the card-day case).
        m.updateEdits([ids[1]], label: "target") { _, e in e.crop.angle = 1.5 }
        #expect(CopyOptions(app: m).changedGroups.isSuperset(of: [.whiteBalance, .spots, .crop, .layers]))
        m.perform(.copyAdjustments)
        #expect(m.pasteOptions.groups == [.whiteBalance], "\(m.pasteOptions.groups)")
        m.selection = Set(ids.dropFirst())
        #expect(m.applyToSelection() == 2)
        for id in ids.dropFirst() {
            #expect(m.edit(for: id).spots.isEmpty && m.edit(for: id).layers.isEmpty)
            #expect(!m.edit(for: id).whiteBalance.asShot)
        }
        #expect(m.edit(for: ids[1]).crop.angle == 1.5, "the target keeps its own straighten")
    }

    /// Selections saved before N6 held crop/spots/layers only because they were preselected.
    @Test func legacySelectionsDropPerPhotoGroups() {
        let suite = testSuiteName()
        let d = UserDefaults(suiteName: suite)!
        d.removePersistentDomain(forName: suite)
        d.set(["crop", "exposure", "spots"], forKey: "rememberedCopyGroups")
        let s = AppSettings(defaults: d)
        #expect(s.copyGroups == [.exposure])
        s.copyGroups = [.crop]
        let r = AppSettings(defaults: d)
        #expect(r.copyGroups == [.crop], "new ticks survive")
        #expect(d.object(forKey: "rememberedCopyGroups") == nil, "the legacy value doesn't come back")
        d.removePersistentDomain(forName: suite)
    }

    // MARK: N7

    @Test func commandOnASelectionIsOneUndoStepOnEveryPhoto() async throws {
        let m = await Nl9AppTests.app()
        let ids = m.visibleIDs
        m.viewer.setViewerSize(CGSize(width: 1200, height: 800), backingScale: 1)
        m.focus(ids[0], extend: false)
        m.selection = Set(ids.prefix(3))
        // Two photos have a horizon, one doesn't.
        m.horizonDetector = PerPhotoHorizon(degrees: [ids[0]: 2, ids[1]: -1.5])
        let depth = m.history.undoStack.count
        let toast = try #require(await m.runInstruction("warmer, crop 4:5 and straighten"))
        #expect(m.history.undoStack.count == depth + 1, "one undo step")
        for id in ids.prefix(3) {
            let e = m.edit(for: id)
            #expect(!e.whiteBalance.asShot && e.crop.aspect == .r4x5 && e.crop.rect != CGRect(x: 0, y: 0, width: 1, height: 1))
        }
        #expect(m.edit(for: ids[0]).crop.angle == -2 && m.edit(for: ids[1]).crop.angle == 1.5 && m.edit(for: ids[2]).crop.angle == 0)
        #expect(m.edit(for: ids[3]) == EditState(), "not selected")
        #expect(toast.message.contains("on 3 photos"), "\(toast.message)")
        #expect(toast.message.contains("no clear horizon on 1 — not straightened"), "\(toast.message)")
        m.undo()
        #expect(ids.prefix(3).allSatisfy { m.edit(for: $0) == EditState() }, "one ⌘Z undoes it all")
    }

    @Test func singlePhotoCompoundSaysWhenTheHorizonWasMissing() async throws {
        let m = await Nl9AppTests.app(horizon: nil)
        let id = try #require(m.focusedID)
        m.viewer.setViewerSize(CGSize(width: 1200, height: 800), backingScale: 1)
        let depth = m.history.undoStack.count
        let toast = try #require(await m.runInstruction("warmer, crop 4:5 and straighten"))
        #expect(m.history.undoStack.count == depth + 1)
        #expect(m.edit(for: id).crop.aspect == .r4x5 && m.edit(for: id).crop.angle == 0)
        #expect(toast.message.contains("K") && toast.message.contains("crop 4:5"), "\(toast.message)")
        #expect(toast.message.contains("no clear horizon — not straightened") && !toast.message.contains("photos"),
                "\(toast.message)")
        // Reset on a selection resets every selected photo.
        m.selection = Set(m.visibleIDs.prefix(2))
        m.updateEdits([m.visibleIDs[1]], label: "x") { _, e in e.exposure.ev = 1 }
        let reset = try #require(await m.runInstruction("reset"))
        #expect(reset.message.hasPrefix("Reset 2 photos") && m.visibleIDs.prefix(2).allSatisfy { m.edit(for: $0) == EditState() })
    }

    @Test func toastWording() {
        #expect(AppModel.instructionToast(summary: "+0.3 EV", photos: 35, targets: 35, unrecognized: []).message
                == "+0.3 EV on 35 photos. ⌘Z to undo")
        #expect(AppModel.instructionToast(summary: "", targets: 3, unlevelled: 3, unrecognized: []).message
                == "No clear horizon found; nothing was straightened")
        #expect(AppModel.instructionToast(summary: "crop 4:5", unlevelled: 1, unrecognized: ["x"]).message
                == "crop 4:5, no clear horizon — not straightened. ⌘Z to undo. Didn't understand: “x”")
    }

    // MARK: N8

    @Test func corruptPresetsAreBackedUpAndSalvaged() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-presets-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("presets.json")
        let store = PresetStore(fileURL: url)
        var edit = EditState()
        edit.exposure.ev = 0.5
        for name in ["One", "Two", "Three"] { store.save(name: name, edit: edit, groups: [.exposure]) }
        // Truncate mid-way through the third preset.
        let full = try Data(contentsOf: url)
        let third = try #require(full.range(of: Data("\"Three\"".utf8)))
        let cut = full.prefix(third.lowerBound)
        try cut.write(to: url)

        let reloaded = PresetStore(fileURL: url)
        #expect(reloaded.userPresets.map(\.name) == ["One", "Two"], "readable presets survive")
        #expect(reloaded.lastError == "1 preset couldn’t be read; the file was kept as presets.json.bak.")
        let bak = dir.appendingPathComponent("presets.json.bak")
        #expect(try Data(contentsOf: bak) == cut, "the backup holds the original bytes")

        reloaded.save(name: "Four", edit: edit, groups: [.exposure])
        #expect(reloaded.lastError?.contains("presets.json.bak") == true, "a save doesn't hide the problem")
        #expect(PresetStore(fileURL: url).userPresets.map(\.name) == ["One", "Two", "Four"])
        #expect(try Data(contentsOf: bak) == cut, "the backup isn't overwritten")

        // A second, different damaged file gets its own backup.
        try Data("{\"presets\": [{\"broken\": 1}]".utf8).write(to: url)
        let again = PresetStore(fileURL: url)
        #expect(again.userPresets.isEmpty && again.lastError?.contains("presets.json.bad-") == true, "\(again.lastError ?? "")")
        #expect(try Data(contentsOf: bak) == cut)
    }

    @Test func salvageReadsEscapedStringsAndStopsAtTheArrayEnd() {
        let (none, zero) = PresetStore.salvage(Data("garbage".utf8))
        #expect(none.isEmpty && zero == 0)
        let (empty, n) = PresetStore.salvage(Data(#"{"presets": [{"name": "a \"}\" b"}, {"x": 1}], "tail": {"y": 2}}"#.utf8))
        #expect(empty.isEmpty && n == 2, "both objects are counted, the one after the array isn't")
    }

    // MARK: N12, N13, Auto

    @Test func whiteBalanceComesFirstOnColor() {
        #expect(ToolTab.color.panels.first == .whiteBalance && ToolTab.color.panels.suffix(2) == [.styles, .history])
        #expect(makeModel().collapsedPanels.isSuperset(of: [.styles, .history]), "Styles & Presets and History start collapsed")
    }

    @Test func renderErrorClearsWhenFocusMoves() async {
        let r = FakeRenderer()
        r.failWith = MockError.invalid("This RAW file is damaged")
        let m = makeModel(renderer: r)
        await m.start()
        #expect(await waitUntil { m.viewer.renderError != nil })
        r.failWith = nil
        m.perform(.move(.right, extend: false))
        // Checked synchronously: the next photo's render (async) can't have landed yet.
        #expect(m.viewer.renderError == nil, "no stale error over the next photo")
        #expect(await waitUntil { m.viewer.frame != nil && m.viewer.renderError == nil })
    }

    /// At the 200-step history limit the undo depth stops growing; the count mustn't.
    @Test func applyCountsChangesAtTheHistoryLimit() async throws {
        let m = makeModel()
        await m.start()
        for i in 0..<(m.history.limit + 5) {
            m.updateEdit(label: "step \(i)") { $0.exposure.ev = Double(i % 7) * 0.1 + 0.1 }
        }
        #expect(m.history.undoStack.count == m.history.limit)
        m.updateEdit(label: "wb") { $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 6100, tint: 0)) }
        m.copyAdjustments()
        let targets = Array(m.visibleIDs.dropFirst().prefix(3))
        #expect(m.applyClipboard(to: targets, options: PasteOptions(groups: [.whiteBalance])) == 3)
        #expect(m.toast?.message == "Applied to 3 photos")
        #expect(m.applyClipboard(to: targets, options: PasteOptions(groups: [.whiteBalance])) == 0, "already applied")
    }

    // MARK: Live straighten (display transform)

    /// Snapshots place the oriented frame as one rotation about the center; that must be
    /// exactly the shader's `toDisplay` (turns then straighten, rigid in pixels).
    @Test func displayRotationMatchesToDisplay() {
        let oriented = CGSize(width: 6000, height: 4000)
        for (turns, angle) in [(0, 0.0), (0, -3.0), (1, 2.5), (2, -7.0), (3, 12.0)] {
            let t = ViewTransform(quarterTurns: turns, angle: angle)
            let display = t.displaySize(oriented: oriented)
            #expect(t.orientedSize(display: display) == oriented)
            let aspect = display.width / display.height
            let rot = CGAffineTransform(rotationAngle: t.displayRotationDegrees * .pi / 180) // clockwise, y down
            for o in [CGPoint(x: 0.1, y: 0.2), CGPoint(x: 0.9, y: 0.35), CGPoint(x: 0.5, y: 0.95)] {
                let p = CGPoint(x: (o.x - 0.5) * oriented.width, y: (o.y - 0.5) * oriented.height).applying(rot)
                let viaRotation = CGPoint(x: p.x / display.width + 0.5, y: p.y / display.height + 0.5)
                let shader = t.toDisplay(o, displayAspect: aspect)
                #expect(abs(viaRotation.x - shader.x) < 1e-9 && abs(viaRotation.y - shader.y) < 1e-9,
                        "turns \(turns) angle \(angle): \(viaRotation) vs \(shader)")
            }
        }
    }

    @Test func onlyCropChangesAreDisplayOnlyInTheOrientedView() {
        let dev = RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true, rendersGeometry: true)
        let other = RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true)
        let id = PhotoID(1)
        var e = EditState()
        e.exposure.ev = 0.3
        var tilted = e
        tilted.crop.angle = -3
        tilted.crop.rect = CGRect(x: 0.1, y: 0.1, width: 0.8, height: 0.8)
        var toned = tilted
        toned.exposure.ev = 0.5
        #expect(ViewerModel.isDisplayOnlyChange(from: (id, e), to: tilted, photo: id, developed: false, capabilities: dev))
        #expect(!ViewerModel.isDisplayOnlyChange(from: (id, e), to: toned, photo: id, developed: false, capabilities: dev))
        #expect(!ViewerModel.isDisplayOnlyChange(from: (id, e), to: tilted, photo: id, developed: true, capabilities: dev),
                "developed display renders the crop")
        #expect(!ViewerModel.isDisplayOnlyChange(from: (id, e), to: tilted, photo: id, developed: false, capabilities: other))
        #expect(!ViewerModel.isDisplayOnlyChange(from: (id, e), to: tilted, photo: PhotoID(2), developed: false, capabilities: dev))
        #expect(!ViewerModel.isDisplayOnlyChange(from: nil, to: tilted, photo: id, developed: false, capabilities: dev))
        #expect(!ViewerModel.isDisplayOnlyChange(from: (id, e), to: e, photo: id, developed: false, capabilities: dev))
    }

    /// The rotation slider in the crop tool: each tick turns the displayed frame at once
    /// (the view transform), with no develop render until the drag settles.
    @Test func straightenSliderRotatesTheDisplayWithoutRendering() async {
        let r = Vw8GeometryRenderer()
        let m = makeViewerApp(renderer: r, count: 6)
        m.viewer.settleDelay = .milliseconds(400)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        m.perform(.tool(.crop))
        #expect(await waitUntil { m.viewer.frame != nil && m.viewer.renderedQuality == .preview && r.requests.last?.geometry == .oriented })
        let n = r.requests.count
        for i in 1...5 {
            m.updateEdit(label: "Straighten", coalesceKey: "angle") { $0.crop.angle = -Double(i) * 0.6 }
            #expect(m.viewer.transform.angle == -Double(i) * 0.6, "the display turns with each tick")
        }
        try? await Task.sleep(for: .milliseconds(150)) // renders start async; still before the settle
        #expect(r.requests.count == n, "no develop render per tick")
        m.endEditGesture()
        #expect(m.viewer.cropRect.width < 1, "the box still shows the auto-fitted crop")
        // A tone change still renders.
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.4 }
        #expect(await waitUntil { r.requests.count > n })
    }

    @Test func autoToastSaysWhatChanged() async throws {
        let m = makeModel()
        await m.start()
        await m.autoAdjust()
        let message = try #require(m.toast?.message)
        #expect(message.hasPrefix("Auto: ") && message.contains("+0.3 EV") && message.contains("→5200 K"), "\(message)")
    }
}
