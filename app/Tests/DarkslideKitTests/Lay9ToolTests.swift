import CoreGraphics
import CoreML
import Foundation
import Testing
@testable import DarkslideAI
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Colour-editor tools, heal/clone, layer tools and brushes, gradient handles, and
/// the new ds-core fields through the sidecar codec.
@MainActor
@Suite struct Lay9ToolTests {
    @Test func colourMathAndTargetedWeights() {
        #expect(ColorPick.hueSaturation(SIMD3(1, 0, 0)) == (0, 1))
        #expect(ColorPick.hueSaturation(SIMD3(0, 1, 0)).hue == 120)
        #expect(ColorPick.hueSaturation(SIMD3(0, 0, 1)).hue == 240)
        #expect(ColorPick.hueSaturation(SIMD3(1, 0, 1)).hue == 300)
        #expect(ColorPick.hueSaturation(SIMD3(0.5, 0.5, 0.5)) == (0, 0))
        #expect(ColorPick.hueSaturation(SIMD3(0, 0, 0)) == (0, 0))
        #expect(ColorPick.range(for: SIMD3(0, 0.8, 0)).width == 30 && ColorPick.range(for: SIMD3(0.5, 0.6, 0.5)).width == 50)
        var old = SkinToneEdit(hueUniformity: 40)
        old.range.width = 20
        let skin = ColorPick.skin(for: SIMD3(0.9, 0.6, 0.5), keeping: old)
        #expect(skin.hueUniformity == 40 && skin.range.width == 40 && skin.range.centerHue > 10 && skin.range.centerHue < 30)
        #expect(ColorPick.hueDistance(350, 10) == 20 && ColorPick.hueDistance(90, 270) == 180)

        let w = TargetedAdjust.weights(hue: 45)
        #expect(w.count == 2 && w[0].0 == 1 && w[1].0 == 2 && abs(w[0].1 - 0.5) < 1e-9)
        #expect(TargetedAdjust.weights(hue: 120).map(\.0) == [3], "exactly on a band")
        let wrap = TargetedAdjust.weights(hue: 330)
        #expect(wrap.map(\.0) == [7, 0] && abs(wrap[0].1 - 0.5) < 1e-9, "magenta wraps to red")
        #expect(TargetedAdjust.weights(hue: -30).map(\.0) == [7, 0])
        var e = EditState()
        e.colorEditor.bands[1].saturation = 95
        let s = TargetedAdjust.apply(hue: 30, amount: 40, channel: .saturation, to: e)
        #expect(s.colorEditor.bands[1].saturation == 100, "clamped")
        let h = TargetedAdjust.apply(hue: 45, amount: 100, channel: .hue, to: EditState())
        #expect(h.colorEditor.bands[1].hueShift == 15 && h.colorEditor.bands[2].hueShift == 15)
        let l = TargetedAdjust.apply(hue: 240, amount: -50, channel: .lightness, to: EditState())
        #expect(l.colorEditor.bands[5].lightness == -50)
        #expect(abs(TargetedAdjust.amount(from: CGPoint(x: 0, y: 0.5), to: CGPoint(x: 0, y: 0.4)) - 20) < 1e-9)
        #expect(TargetedAdjust.amount(from: CGPoint(x: 0, y: 0), to: CGPoint(x: 0, y: 1)) == -100)

        let r = [ColorRangeEdit(centerHue: 1), ColorRangeEdit(centerHue: 2), ColorRangeEdit(centerHue: 3)]
        #expect(ColorRangeOps.removing(1, from: r).map(\.centerHue) == [1, 3] && ColorRangeOps.removing(9, from: r) == r)
        #expect(ColorRangeOps.selection(afterRemoving: 2, count: 3) == 1 && ColorRangeOps.selection(afterRemoving: 0, count: 1) == nil)
        #expect([1, 2][safe: 5] == nil && [1, 2][safe: 1] == 2)
    }

    /// The viewer with a develop frame whose pixels are orange-ish (FakeRenderer).
    func viewer() async -> (AppModel, (CGFloat, CGFloat) -> CGPoint) {
        let m = makeModel()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        _ = await waitUntil { m.viewer.frame != nil }
        let img = m.viewer.layout.main!.image
        return (m, { x, y in CGPoint(x: img.minX + x * img.width, y: img.minY + y * img.height) })
    }

    func click(_ m: AppModel, _ p: CGPoint) {
        m.viewer.beginCanvasDrag(at: p)
        m.viewer.endCanvasDrag()
    }

    @Test func pickersTargetedDragAndEscapeOnTheImage() async {
        let (m, at) = await viewer()
        m.colorTools.mode = .pickRange
        #expect(m.viewer.activeTool == .colorPicker)
        click(m, at(0.5, 0.5))
        #expect(m.focusedEdit.colorEditor.advanced.count == 1 && m.colorTools.mode == nil && m.colorTools.selectedRange == 0)
        let hue = m.focusedEdit.colorEditor.advanced[0].centerHue
        #expect(hue > 5 && hue < 40, "the frame is orange: \(hue)")
        #expect(m.history.undoLabel == "Add Color Range")

        m.colorTools.mode = .pickSkin
        click(m, at(0.4, 0.4))
        #expect(m.focusedEdit.colorEditor.skin != nil && m.colorTools.mode == nil)

        m.colorTools.mode = .targeted(.saturation)
        let depth = m.history.undoStack.count
        m.viewer.beginCanvasDrag(at: at(0.5, 0.6))
        m.viewer.updateCanvasDrag(to: at(0.5, 0.5))
        m.viewer.updateCanvasDrag(to: at(0.5, 0.4))
        m.viewer.endCanvasDrag()
        let bands = m.focusedEdit.colorEditor.bands
        #expect(bands[0].saturation + bands[1].saturation > 25, "orange bands up: \(bands.map(\.saturation))")
        #expect(m.history.undoStack.count == depth + 1, "one undo step per drag")
        #expect(m.colorTools.mode == .targeted(.saturation), "the targeted tool stays until Esc")
        m.perform(.escape)
        #expect(m.colorTools.mode == nil)

        // A click off the image does nothing but keeps the tool.
        m.colorTools.mode = .pickRange
        click(m, CGPoint(x: -500, y: -500))
        #expect(m.focusedEdit.colorEditor.advanced.count == 1)
        m.colorTools.cancel()
    }

    @Test func maskColourRangeHealAndClone() async {
        let (m, at) = await viewer()
        // Without a selected layer the mask-range picker just ends.
        m.colorTools.mode = .pickMaskRange
        click(m, at(0.5, 0.5))
        #expect(m.colorTools.mode == nil)
        let layers = LayerController(app: m)
        layers.add()
        m.colorTools.mode = .pickMaskRange
        click(m, at(0.5, 0.5))
        #expect(layers.selectedLayer?.mask.colorRange != nil)
        layers.clearColorRange()
        #expect(layers.selectedLayer?.mask.colorRange == nil)
        layers.setLumaRange(true)
        #expect(layers.selectedLayer?.mask.lumaRange == LumaRangeEdit())
        layers.setLumaRange(false)
        #expect(layers.selectedLayer?.mask.lumaRange == nil)
        layers.updateTools(label: "Layer Vibrance", key: "vibrance") { $0.presence.vibrance = 30 }
        #expect(layers.selectedLayer?.tools.presence.vibrance == 30)

        // Heal: a click finds a source; clone: a drag sets it.
        m.colorTools.mode = .spot(.heal)
        click(m, at(0.5, 0.5))
        var spots = m.focusedEdit.spots
        #expect(spots.count == 1 && spots[0].kind == .heal && spots[0].source != spots[0].center)
        m.colorTools.mode = .spot(.clone)
        m.viewer.beginCanvasDrag(at: at(0.3, 0.3))
        m.viewer.updateCanvasDrag(to: at(0.6, 0.3))
        m.viewer.endCanvasDrag()
        spots = m.focusedEdit.spots
        #expect(spots.count == 2 && spots[1].kind == .clone && spots[1].source.x > spots[1].center.x + 0.2)
        #expect(m.history.undoLabel == "Clone Spot")
        m.colorTools.spotRadius = 0.02
        m.perform(.brushSize(1))
        #expect(abs(m.colorTools.spotRadius - 0.025) < 1e-9, "[ ] size spots while the spot tool is on")
        m.colorTools.cancel()
    }

    @Test func autoSourcePicksTheBestMatchingPatchInsideTheImage() {
        // A bright area to the right, dark everywhere else: the ring is dark, so the
        // source must be a dark patch, never the bright one.
        let sample: (CGPoint, Double) -> SIMD3<Double>? = { p, _ in
            p.x > 0.55 ? SIMD3(1, 1, 1) : SIMD3(0.1, 0.1, 0.1)
        }
        let src = SpotSource.auto(center: CGPoint(x: 0.5, y: 0.5), radius: 0.02, aspect: 1.5, sample: sample)
        #expect(src.x <= 0.55 && src != CGPoint(x: 0.5, y: 0.5))
        #expect(SpotSource.candidates(center: CGPoint(x: 0.01, y: 0.5), radius: 0.02, aspect: 1).allSatisfy { $0.x > 0 })
        let none = SpotSource.auto(center: CGPoint(x: 0.5, y: 0.5), radius: 0.02, aspect: 1) { _, _ in nil }
        #expect(none.x > 0.5, "fallback: to the right")
        let (dx, dy) = SpotSource.extents(radius: 0.1, aspect: 0.5)
        #expect(dx == 0.2 && dy == 0.1)
    }

    @Test func brushSettingsKeysAndGradientHandles() async {
        let s = MaskShape.brush(.add, [BrushStroke(points: [.zero], erase: false), BrushStroke(points: [.zero], erase: true)])
        guard case .brush(_, let st) = s.applyingBrush(hardness: 0.9, flow: 0.4, autoMask: true) else { Issue.record("brush"); return }
        #expect(st[0].hardness == 0.9 && st[0].flow == 0.4 && st[0].autoMask && !st[1].autoMask, "erasing never auto-masks")
        #expect(MaskShape.fill(.add).applyingBrush(hardness: 1, flow: 1, autoMask: true) == .fill(.add))
        #expect(BrushSteps.step(0.05, by: 1, range: 0.005...0.5) == 0.0625 && BrushSteps.step(0.5, by: 3, range: 0.005...0.5) == 0.5)
        #expect(KeyboardRouter.command(for: KeyInput("[")) == .brushSize(-1) && KeyboardRouter.command(for: KeyInput("]")) == .brushSize(1))

        let (m, at) = await viewer()
        m.viewer.canvasTool = .brush
        m.perform(.brushSize(-1))
        #expect(abs(m.viewer.brushSize - 0.04) < 1e-9)
        m.viewer.canvasTool = .pan
        m.perform(.brushSize(1))
        #expect(abs(m.viewer.brushSize - 0.04) < 1e-9, "no brush tool: no change")
        // Strokes get the strip's settings.
        m.viewer.brushHardness = 0.8
        m.viewer.brushFlow = 0.3
        m.viewer.brushAutoMask = true
        m.viewer.canvasTool = .brush
        m.viewer.beginCanvasDrag(at: at(0.2, 0.2))
        m.viewer.updateCanvasDrag(to: at(0.3, 0.3))
        m.viewer.endCanvasDrag()
        guard case .brush(_, let strokes)? = m.focusedEdit.layers.first?.mask.shapes.first else { Issue.record("no stroke"); return }
        #expect(strokes[0].hardness == 0.8 && strokes[0].flow == 0.3 && strokes[0].autoMask)
        // A linear gradient, then drag its end handle.
        m.viewer.canvasTool = .linearGradient
        m.viewer.beginCanvasDrag(at: at(0.5, 0.1))
        m.viewer.updateCanvasDrag(to: at(0.5, 0.5))
        m.viewer.endCanvasDrag()
        #expect(m.focusedEdit.layers[0].mask.shapes.count == 2)
        let depth = m.history.undoStack.count
        m.viewer.beginCanvasDrag(at: at(0.5, 0.5))
        m.viewer.updateCanvasDrag(to: at(0.5, 0.7))
        m.viewer.updateCanvasDrag(to: at(0.5, 0.8))
        m.viewer.endCanvasDrag()
        #expect(m.focusedEdit.layers[0].mask.shapes.count == 2, "the handle edits, it doesn't add a gradient")
        guard case .linear(_, let a, let b) = m.focusedEdit.layers[0].mask.shapes[1] else { Issue.record("linear"); return }
        #expect(abs(b.y - 0.8) < 0.02 && abs(a.y - 0.1) < 0.02, "\(a) \(b)")
        #expect(m.history.undoStack.count == depth + 1)
    }

    @Test func handleMathForRadialGradients() {
        let r = MaskShape.radial(.add, center: CGPoint(x: 0.5, y: 0.5), radius: CGSize(width: 0.2, height: 0.1), angle: 0, feather: 0.3)
        let hs = MaskHandles.handles(of: r, aspect: 1)
        #expect(hs.count == 3 && hs[1].1 == CGPoint(x: 0.7, y: 0.5))
        #expect(abs(hs[2].1.y - 0.6) < 1e-9)
        #expect(MaskHandles.hit(CGPoint(x: 0.705, y: 0.5), shapes: [.fill(.add), r], aspect: 1, tolerance: 0.01)! == (1, .radialEdgeX))
        #expect(MaskHandles.hit(CGPoint(x: 0.9, y: 0.9), shapes: [r], aspect: 1, tolerance: 0.01) == nil)
        guard case .radial(_, let c, _, _, _) = MaskHandles.move(r, handle: .radialCenter, to: CGPoint(x: 0.4, y: 0.4), aspect: 1) else { return }
        #expect(c == CGPoint(x: 0.4, y: 0.4))
        guard case .radial(_, _, let rx, let angle, let f) = MaskHandles.move(r, handle: .radialEdgeX, to: CGPoint(x: 0.5, y: 0.8), aspect: 1) else { return }
        #expect(abs(rx.width - 0.3) < 1e-9 && abs(angle - 90) < 1e-9 && f == 0.3)
        guard case .radial(_, _, let ry, _, _) = MaskHandles.move(r, handle: .radialEdgeY, to: CGPoint(x: 0.5, y: 0.55), aspect: 1) else { return }
        #expect(abs(ry.height - 0.05) < 1e-9 && ry.width == 0.2)
        #expect(MaskHandles.move(.fill(.add), handle: .linearEnd, to: .zero, aspect: 1) == .fill(.add))
        guard case .linear(_, let a, _) = MaskHandles.move(.linear(.add, start: .zero, end: CGPoint(x: 1, y: 1)), handle: .linearStart,
                                                          to: CGPoint(x: 0.2, y: 0.2), aspect: 1) else { return }
        #expect(a == CGPoint(x: 0.2, y: 0.2))
    }

    @Test func newCoreFieldsRoundTripThroughTheSidecarCodec() throws {
        var e = EditState()
        e.presence = PresenceEdit(vibrance: 20, texture: -10, dehaze: 15)
        e.spots = [SpotEdit(kind: .clone, center: CGPoint(x: 0.3, y: 0.4), radius: 0.01, source: CGPoint(x: 0.35, y: 0.4),
                            feather: 0.3, opacity: 0.9)]
        e.colorEditor.advanced = [ColorRangeEdit(centerHue: 25, width: 30, smoothness: 0.2, hueShift: 4, saturation: -10, lightness: 5)]
        e.colorEditor.skin = SkinToneEdit(hueUniformity: 30, saturationUniformity: 20, lightnessUniformity: 10)
        var l = LayerEdit(id: 3, name: "Sky", exposureEV: -0.5)
        l.tools.temperature = -400
        l.tools.tint = 5
        l.tools.contrast = 10
        l.tools.presence.dehaze = 25
        l.tools.hdr.highlights = -30
        l.tools.colorBalance.shadows = WheelValue(hue: 200, saturation: 0.1, lightness: 0)
        l.tools.curve.points[.luma] = [CurvePoint(0.5, 0.45)]
        l.tools.colorRanges = [ColorRangeEdit(centerHue: 220, saturation: 20)]
        l.tools.clarity = 12
        l.tools.structure = -4
        l.tools.sharpening = -40
        l.mask = MaskEdit(shapes: [.brush(.add, [BrushStroke(points: [CGPoint(x: 0.1, y: 0.2)], autoMask: true)])], invert: true,
                          feather: 0.05, lumaRange: LumaRangeEdit(low: 0.2, high: 0.9), colorRange: ColorRangeEdit(centerHue: 210))
        e.layers = [l]
        let json = try AdjustmentsCodec.encode(e)
        let (back, raw) = try AdjustmentsCodec.decode(json)
        #expect(back == e)
        // ds-core names on the wire.
        let layer = (raw["layers"] as! [[String: Any]])[0]
        let adj = layer["adjustments"] as! [String: Any]
        #expect((adj["presence"] as! [String: Any])["dehaze"] as! Double == 25)
        #expect((adj["white_balance"] as! [String: Any])["temp_k"] as! Double == -400)
        #expect(adj["sharpening"] as! Double == -40)
        #expect((layer["mask"] as! [String: Any])["luma_range"] is [String: Any])
        #expect((raw["spots"] as! [[String: Any]])[0]["kind"] as! String == "clone")
        #expect(((raw["color_editor"] as! [String: Any])["skin"] as! [String: Any])["hue_uniformity"] as! Double == 30)
        // A layer without a WB shift writes none; no skin writes null.
        var plain = EditState()
        plain.layers = [LayerEdit(id: 0, name: "x")]
        let (p, praw) = try AdjustmentsCodec.decode(AdjustmentsCodec.encode(plain))
        #expect(p == plain)
        #expect(((praw["layers"] as! [[String: Any]])[0]["adjustments"] as! [String: Any])["white_balance"] is NSNull)
        #expect((praw["color_editor"] as! [String: Any])["skin"] is NSNull)
    }

    @Test func perShapeFeathersSurviveEditsTheAppDoesntModel() throws {
        // A sidecar from a newer writer: an AI shape with a feather.
        let json = #"{"layers":[{"id":1,"name":"S","mask":{"shapes":[{"kind":"ai","op":"add","target":"sky","feather":0.25}],"invert":false,"feather":0}}]}"#
        let (e, raw) = try AdjustmentsCodec.decode(json)
        #expect(e.layers[0].mask.shapes == [.ai(.add, .sky)])
        var changed = e
        changed.layers[0].mask.shapes.append(.fill(.subtract))
        let out = try AdjustmentsCodec.decode(AdjustmentsCodec.encode(changed, onto: raw)).1
        let shapes = ((out["layers"] as! [[String: Any]])[0]["mask"] as! [String: Any])["shapes"] as! [[String: Any]]
        #expect(shapes.count == 2 && shapes[0]["feather"] as? Double == 0.25, "kept shape keeps its feather")
    }

    @Test func presenceAndSpotsAreCopyGroups() {
        #expect(AdjustGroup.allCases.count == 17 && AdjustGroup.allCases[3] == .presence && AdjustGroup.allCases.last == .spots)
        #expect(AdjustGroup.presence.supportsRelative && !AdjustGroup.spots.supportsRelative)
        #expect(AdjustGroup.presence.title == "Presence" && AdjustGroup.spots.title == "Heal & Clone")
        #expect(!AdjustGroup.presence.symbol.isEmpty && !AdjustGroup.spots.symbol.isEmpty)
        var src = EditState()
        src.presence.vibrance = 20
        src.spots = [SpotEdit()]
        var target = EditState()
        target.presence.vibrance = 10
        let clip = AdjustmentClipboard(source: src)
        let rel = clip.apply(to: target, options: PasteOptions(groups: [.presence, .spots], relative: [.presence]))
        #expect(rel.presence.vibrance == 30 && rel.spots.count == 1)
        let abs = clip.apply(to: target, options: PasteOptions(groups: [.presence]))
        #expect(abs.presence.vibrance == 20 && abs.spots.isEmpty)
        #expect(AdjustGroup.changed(in: src) == [.presence, .spots])
        #expect(ToolPanelID.presence.group == .presence && ToolPanelID.spots.group == .spots)
        #expect(ToolTab.exposure.panels.contains(.presence) && ToolTab.details.panels.contains(.spots))
        #expect(AdjustGroup(rust: AdjustGroup.presence.rust) == .presence && AdjustGroup(rust: AdjustGroup.spots.rust) == .spots)
        var a = SpotEdit()
        let b = SpotEdit()
        #expect(a == b, "identity isn't part of equality")
        a.opacity = 0.5
        #expect(a != b)
    }
}

/// The sky model: download with checksums and progress, compile, and the class mask.
@MainActor
@Suite(.serialized) struct Lay9SkyTests {
    /// Serves files from a local folder (keyed by the URL's last path components).
    struct LocalDownloader: ModelDownloader {
        let files: [String: Data]
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            guard let data = files[url.lastPathComponent] else { throw URLError(.fileDoesNotExist) }
            try data.write(to: destination)
            progress(Int64(data.count))
        }
    }

    static func descriptor(_ files: [String: Data]) -> DownloadableModelDescriptor {
        DownloadableModelDescriptor(name: "Fake", displayName: "sky model", files: files.keys.sorted().map { k in
            DownloadableModelDescriptor.File(path: "Data/\(k)", url: URL(string: "https://example.invalid/\(k)")!,
                                             sha256: sha(files[k]!), size: Int64(files[k]!.count))
        }, computeUnits: .cpuAndNeuralEngine, environmentKey: "DS_FAKE_MODEL_DIR")
    }

    static func sha(_ d: Data) -> String {
        let t = FileManager.default.temporaryDirectory.appendingPathComponent("sha-\(UUID())")
        try! d.write(to: t)
        defer { try? FileManager.default.removeItem(at: t) }
        return try! ModelFiles.sha256(of: t)
    }

    func root() -> URL { FileManager.default.temporaryDirectory.appendingPathComponent("ds-lay9-models-\(UUID())") }

    @Test func downloadsVerifiesCompilesOnceAndReportsProgress() async throws {
        let files = ["a.bin": Data(repeating: 1, count: 3000), "b.json": Data("{}".utf8)]
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let compiled = Lay9Locked<Int>(0)
        let s = SkySegmentation(root: dir, descriptor: Self.descriptor(files), downloader: LocalDownloader(files: files),
                                compile: { pkg in
                                    compiled.withLock { $0 += 1 }
                                    #expect(FileManager.default.fileExists(atPath: pkg.appendingPathComponent("Data/a.bin").path))
                                    let out = pkg.deletingLastPathComponent().appendingPathComponent("out.mlmodelc")
                                    try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
                                    return out
                                })
        #expect(s.state == .notDownloaded)
        let url = try await s.prepare()
        #expect(url == s.compiledURL && s.state == .ready && FileManager.default.fileExists(atPath: url.path))
        _ = try await s.prepare()
        #expect(compiled.withLock { $0 } == 1, "compiled once")
        // No staging folders left behind.
        let left = try FileManager.default.contentsOfDirectory(atPath: dir.path).filter { $0.contains("download") }
        #expect(left.isEmpty)
        #expect(SkySegmentation(root: dir, descriptor: Self.descriptor(files)).state == .ready, "found on the next launch")
    }

    @Test func aDamagedDownloadOrAFailedCompileIsReported() async {
        let good = ["a.bin": Data(repeating: 1, count: 100)]
        var bad = Self.descriptor(good)
        bad.files[0].sha256 = String(repeating: "0", count: 64)
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = SkySegmentation(root: dir, descriptor: bad, downloader: LocalDownloader(files: good), compile: { $0 })
        await #expect(throws: ModelDownloadError.checksum("sky model", "Data/a.bin")) { try await s.prepare() }
        guard case .failed(let msg) = s.state else { Issue.record("\(s.state)"); return }
        #expect(msg.contains("damaged"))
        let missing = SkySegmentation(root: root(), descriptor: Self.descriptor(good), downloader: LocalDownloader(files: [:]),
                                      compile: { $0 })
        await #expect(throws: ModelDownloadError.self) { try await missing.prepare() }
        struct Boom: Error {}
        let noCompile = SkySegmentation(root: root(), descriptor: Self.descriptor(good), downloader: LocalDownloader(files: good),
                                        compile: { _ in throw Boom() })
        do {
            _ = try await noCompile.prepare()
            Issue.record("expected a compile error")
        } catch let e as ModelDownloadError {
            if case .compile = e {} else { Issue.record("\(e)") }
            #expect(e.errorDescription?.contains("prepare") == true)
        } catch { Issue.record("\(error)") }
        for e in [SegmentationError.download("x"), .inference("y")] { #expect(e.errorDescription?.isEmpty == false) }
    }

    @Test func classMaskLabelsAndSoftening() throws {
        let a = try MLMultiArray(shape: [4, 4], dataType: .int32)
        for i in 0..<16 { a[i] = NSNumber(value: i < 8 ? 2 : 5) }
        let m = try #require(SkySegmentation.classMask(a, classIndex: 2))
        #expect(m.width == 4 && m.height == 4 && m.bytes[0] == 255 && m.bytes[15] == 0)
        #expect(m.bytes[4] > 0 && m.bytes[4] < 255 || m.bytes[8] > 0, "edge softened")
        #expect(SkySegmentation.classMask(a, classIndex: 9) == nil, "no sky")
        #expect(SkySegmentation.classIndex(labels: ["--", "person", "sky (other)"], label: "sky") == 2)
        #expect(SkySegmentation.classIndex(labels: [], label: "sky") == nil)
        #expect(SkySegmentation.soften([255, 0, 0, 0], width: 2, height: 2) == [63, 63, 63, 63])
        #expect(DownloadableModelDescriptor.sky.totalBytes == 85_468_610 && DownloadableModelDescriptor.sky.files.count == 3)
    }

    /// gpu-audit #3: class ids are read through the typed buffer for every Core ML
    /// output type, and a 448x448 map takes well under a millisecond-scale budget.
    @Test func classMaskReadsTypedBuffersFast() throws {
        for type in [MLMultiArrayDataType.int32, .float16, .float32, .double] {
            let a = try MLMultiArray(shape: [4, 4], dataType: type)
            for i in 0..<16 { a[i] = NSNumber(value: i < 8 ? 2 : 5) }
            let m = try #require(SkySegmentation.classMask(a, classIndex: 2), "\(type.rawValue)")
            #expect(m.bytes[0] == 255 && m.bytes[15] == 0, "\(type.rawValue)")
        }
        let big = try MLMultiArray(shape: [448, 448], dataType: .int32)
        let p = big.dataPointer.bindMemory(to: Int32.self, capacity: 448 * 448)
        for i in 0..<(448 * 448) { p[i] = i < 448 * 200 ? 2 : 7 }
        let clock = ContinuousClock()
        let t = clock.now
        let m = try #require(SkySegmentation.classMask(big, classIndex: 2))
        let ms = Double((clock.now - t).components.attoseconds) / 1e15
        print(String(format: "GPUAUDIT sky class mask 448x448 (incl. 3x3 soften): %.2f ms", ms))
        #expect(m.bytes[0] == 255 && m.bytes[448 * 447] == 0)
    }

    /// The real model on a real sample with sky, when the model files are on this Mac
    /// (`DS_SKY_MODEL_DIR`, a local copy of the `.mlpackage`); no network needed.
    @Test func realModelFindsTheSkyOnASample() async throws {
        guard let env = ProcessInfo.processInfo.environment["DS_SKY_MODEL_DIR"],
              FileManager.default.fileExists(atPath: URL(fileURLWithPath: env).appendingPathComponent("Manifest.json").path) else {
            print("SKIP: realModelFindsTheSkyOnASample — set DS_SKY_MODEL_DIR to a local DETRResnet50SemanticSegmentationF16.mlpackage")
            return
        }
        let local = URL(fileURLWithPath: env)
        guard let sample = sampleARWs().first(where: { $0.lastPathComponent == "SKB08229.ARW" }) else {
            print("SKIP: realModelFindsTheSkyOnASample — sample SKB08229.ARW not found")
            return
        }
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = SkySegmentation(root: dir, downloader: LocalModelDownloader(package: local))
        let preview = try await RustPreviewProvider().preview(for: sample, maxPixelSize: 1024)
        let clock = ContinuousClock(), t0 = clock.now
        let mask = try #require(try await s.mask(for: preview))
        print("LAY9 sky mask: \(mask.width)x\(mask.height), coverage \(mask.coverage), \(clock.now - t0) incl. compile")
        #expect(mask.coverage > 0.05 && mask.coverage < 0.8)
        // The sky is at the top of this landscape: the top rows are mostly sky.
        let top = mask.bytes.prefix(mask.width * mask.height / 8)
        #expect(Double(top.filter { $0 >= 128 }.count) / Double(top.count) > 0.5)
        let bottom = mask.bytes.suffix(mask.width * mask.height / 8)
        #expect(Double(bottom.filter { $0 >= 128 }.count) / Double(bottom.count) < 0.1)
    }

    @Test func urlSessionDownloaderWritesTheFileWithProgress() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let src = dir.appendingPathComponent("src.bin"), out = dir.appendingPathComponent("out.bin")
        let payload = Data((0..<((1 << 20) + 5)).map { UInt8($0 & 0xFF) })
        try payload.write(to: src)
        let seen = Lay9Locked<[Int64]>([])
        try await URLSessionModelDownloader(schemes: ["file"]).download(src, to: out) { n in seen.withLock { $0.append(n) } }
        #expect(try Data(contentsOf: out) == payload)
        let reported = seen.withLock { $0 }
        #expect(reported.last == Int64(payload.count) && reported == reported.sorted(), "\(reported)")
        // An existing file at the destination is replaced.
        try await URLSessionModelDownloader(schemes: ["file"]).download(src, to: out) { _ in }
        #expect(try Data(contentsOf: out) == payload)
    }

    @Test func localDownloaderCopiesByPackagePath() async throws {
        let pkg = root().appendingPathComponent("M.mlpackage")
        defer { try? FileManager.default.removeItem(at: pkg.deletingLastPathComponent()) }
        try FileManager.default.createDirectory(at: pkg.appendingPathComponent("Data"), withIntermediateDirectories: true)
        try Data("x".utf8).write(to: pkg.appendingPathComponent("Data/f.bin"))
        let d = LocalModelDownloader(package: pkg)
        let out = pkg.deletingLastPathComponent().appendingPathComponent("out.bin")
        let got = Lay9Locked<Int64>(0)
        try await d.download(URL(string: "https://h/x/M.mlpackage/Data/f.bin")!, to: out) { n in got.withLock { $0 = n } }
        #expect(try Data(contentsOf: out) == Data("x".utf8) && got.withLock { $0 } == 1)
        await #expect(throws: SegmentationError.self) {
            try await d.download(URL(string: "https://h/nothing.bin")!, to: out) { _ in }
        }
        await #expect(throws: SegmentationError.self) {
            try await d.download(URL(string: "https://h/M.mlpackage/missing.bin")!, to: out) { _ in }
        }
    }

    /// Live: the published model files still exist with the recorded sizes (network only).
    @Test func publishedModelFilesMatchTheDescriptor() async throws {
        let f = DownloadableModelDescriptor.sky.files[0]
        var req = URLRequest(url: f.url)
        req.httpMethod = "GET"
        req.timeoutInterval = 10
        guard let (data, resp) = try? await URLSession.shared.data(for: req), (resp as? HTTPURLResponse)?.statusCode == 200 else {
            print("SKIP: publishedModelFilesMatchTheDescriptor — offline (couldn't fetch \(f.url.lastPathComponent))")
            return
        }
        #expect(Int64(data.count) == f.size)
        let t = FileManager.default.temporaryDirectory.appendingPathComponent("manifest-\(UUID()).json")
        try data.write(to: t)
        defer { try? FileManager.default.removeItem(at: t) }
        #expect(try ModelFiles.sha256(of: t) == f.sha256)
    }
}

private final class Lay9Locked<T>: @unchecked Sendable {
    private var value: T
    private let lock = NSLock()
    init(_ v: T) { value = v }
    func withLock<R>(_ f: (inout T) -> R) -> R { lock.withLock { f(&value) } }
}
