import CoreGraphics
import Foundation
import Testing
@testable import DarkslideKit

/// User request: "when I click the dropper for white balance, change the cursor to look
/// like a magnifying glass so you can really see the white pixel to choose". A
/// Lightroom/Capture One loupe: full-resolution pixels under the cursor, the sample area
/// outlined, and a readout whose Kelvin/tint is exactly what the click applies.
@Suite struct PickerLoupeLogicTests {
    /// An 8-bit frame whose pixel (x, y) is (x*10, y*10, 100), with `white` pixels at 255.
    static func frame(_ w: Int, _ h: Int, white: [(Int, Int)] = []) -> RenderedFrame {
        var px = [UInt8](repeating: 255, count: w * h * 4)
        for y in 0..<h {
            for x in 0..<w {
                let i = (y * w + x) * 4
                px[i] = UInt8(x * 10)
                px[i + 1] = UInt8(y * 10)
                px[i + 2] = 100
            }
        }
        for (x, y) in white { for c in 0..<3 { px[(y * w + x) * 4 + c] = 255 } }
        return RenderedFrame(width: w, height: h, rgba: Data(px))
    }

    static func region(_ f: RenderedFrame, rect: CGRect = CGRect(x: 0, y: 0, width: 1, height: 1),
                       space: CGSize? = nil) -> PickerLoupe.Region {
        PickerLoupe.Region(frame: f, rect: rect, space: space ?? CGSize(width: f.width, height: f.height),
                           photo: PhotoID(1), edit: EditState())
    }

    @Test func measuresTheSampleSquareAndFlagsClipping() throws {
        let r = Self.region(Self.frame(9, 9, white: [(5, 5)]))
        let one = try #require(PickerLoupe.measure(r, at: LoupePixel(x: 3, y: 4), sample: 1))
        #expect(abs(one.rgb.x - 30.0 / 255) < 1e-9 && abs(one.rgb.y - 40.0 / 255) < 1e-9 && !one.clipped)
        let three = try #require(PickerLoupe.measure(r, at: LoupePixel(x: 3, y: 4), sample: 3))
        #expect(abs(three.rgb.x - 30.0 / 255) < 1e-9 && abs(three.rgb.y - 40.0 / 255) < 1e-9, "mean of the 3×3")
        #expect(!three.clipped, "(5, 5) is outside the 3×3 around (3, 4)")
        #expect(PickerLoupe.measure(r, at: LoupePixel(x: 4, y: 4), sample: 3)?.clipped == true)
        #expect(PickerLoupe.measure(r, at: LoupePixel(x: 3, y: 4), sample: 5)?.clipped == true)
        #expect(PickerLoupe.measure(r, at: LoupePixel(x: 9, y: 0), sample: 1) == nil, "outside the region")
    }

    /// A region is a 1:1 window of a larger frame space: pixels map by its rectangle.
    @Test func regionsMapFrameSpacePixels() throws {
        let f = Self.frame(10, 10)
        let r = Self.region(f, rect: CGRect(x: 0.5, y: 0.25, width: 0.1, height: 0.1), space: CGSize(width: 100, height: 100))
        #expect(r.local(LoupePixel(x: 50, y: 25))! == (0, 0))
        #expect(r.local(LoupePixel(x: 57, y: 31))! == (7, 6))
        #expect(r.local(LoupePixel(x: 49, y: 25)) == nil && r.local(LoupePixel(x: 60, y: 30)) == nil)
        #expect(PickerLoupe.measure(r, at: LoupePixel(x: 57, y: 31), sample: 1).map { Int(($0.rgb.x * 255).rounded()) } == 70)
        #expect(r.covers(LoupePixel(x: 55, y: 30), radius: 4) && !r.covers(LoupePixel(x: 55, y: 30), radius: 5))
        // At the frame's edge the region can't extend further: it covers.
        let edge = Self.region(f, rect: CGRect(x: 0.9, y: 0, width: 0.1, height: 0.1), space: CGSize(width: 100, height: 100))
        #expect(edge.covers(LoupePixel(x: 99, y: 0), radius: 4))
    }

    /// Up and to the right of the cursor, never over it; flipped near the edges.
    @Test func placementAvoidsThePickPointAndFlipsAtEdges() {
        let bounds = CGRect(x: 0, y: 0, width: 1200, height: 800)
        let d = PickerLoupe.diameter
        func coversCursor(_ r: CGRect, _ c: CGPoint) -> Bool {
            r.insetBy(dx: -2, dy: -2).contains(c)
                || CGRect(x: r.minX, y: r.maxY, width: r.width, height: PickerLoupe.readoutHeight).contains(c)
        }
        let mid = CGPoint(x: 600, y: 400)
        let r = PickerLoupe.placement(cursor: mid, bounds: bounds)
        #expect(r.minX > mid.x && r.maxY < mid.y && r.width == d, "up-right: \(r)")
        #expect(!coversCursor(r, mid))
        let right = CGPoint(x: 1150, y: 400)
        let rr = PickerLoupe.placement(cursor: right, bounds: bounds)
        #expect(rr.maxX < right.x && bounds.contains(rr) && !coversCursor(rr, right), "flipped left: \(rr)")
        let top = CGPoint(x: 600, y: 30)
        let rt = PickerLoupe.placement(cursor: top, bounds: bounds)
        #expect(rt.minY > top.y && bounds.contains(rt) && !coversCursor(rt, top), "flipped below: \(rt)")
        let corner = CGPoint(x: 1190, y: 10)
        let rc = PickerLoupe.placement(cursor: corner, bounds: bounds)
        #expect(rc.maxX < corner.x && rc.minY > corner.y && !coversCursor(rc, corner))
        let tiny = PickerLoupe.placement(cursor: CGPoint(x: 50, y: 50), bounds: CGRect(x: 0, y: 0, width: 100, height: 100))
        #expect(tiny.minX >= 0 && tiny.minY >= 0, "kept inside a pane too small for either side")
    }

    @Test func neutralityInWords() {
        #expect(PickerLoupe.neutrality(SIMD3(0.5, 0.5, 0.5)) == "Neutral")
        #expect(PickerLoupe.neutrality(SIMD3(0.52, 0.5, 0.48)).hasPrefix("Nearly neutral, slightly warm"))
        #expect(PickerLoupe.neutrality(SIMD3(0.8, 0.6, 0.4)) == "Not neutral: warm cast")
        #expect(PickerLoupe.neutrality(SIMD3(0.4, 0.55, 0.8)) == "Not neutral: cool cast")
        #expect(PickerLoupe.neutrality(SIMD3(0.4, 0.6, 0.45)).hasSuffix("green cast"))
        #expect(PickerLoupe.neutrality(SIMD3(0.6, 0.42, 0.6)).hasSuffix("magenta cast"))
    }

    @MainActor @Test func spokenReadoutAndMagnification() {
        var wb = WhiteBalanceEdit()
        wb.asShot = false
        wb.temperature = 5400
        wb.tint = 3.2
        let r = PickerLoupe.Readout(rgb: SIMD3(201, 199, 203), clipped: true, neutrality: "Neutral", whiteBalance: wb)
        let text = PickerLoupe.spoken(r, kind: .whiteBalance)
        #expect(text.contains("Red 201, green 199, blue 203") && text.contains("5400 Kelvin, tint +3.2"))
        #expect(text.contains("Clipped") && text.contains("Neutral"))
        #expect(!PickerLoupe.spoken(r, kind: .skinTone).contains("Kelvin"))
        var failed = r
        failed.whiteBalance = nil
        failed.whiteBalanceError = "Clipped or black: pick another neutral"
        #expect(PickerLoupe.spoken(failed, kind: .whiteBalance).contains("pick another neutral"))
        let loupe = PickerLoupe()
        let start = loupe.visibleRadius
        loupe.scroll(by: 3)
        #expect(loupe.magnification > 9 && loupe.visibleRadius < start, "scroll up magnifies")
        loupe.scroll(by: -100)
        #expect(loupe.magnification == PickerLoupe.magnificationRange.lowerBound)
        loupe.scroll(by: 100)
        #expect(loupe.magnification == PickerLoupe.magnificationRange.upperBound)
        loupe.scroll(by: .nan)
        #expect(loupe.magnification == PickerLoupe.magnificationRange.upperBound)
    }

    @MainActor @Test func sampleSizeIsASetting() {
        #expect(PickerSample.allCases.map(\.pixels) == [1, 3, 5, 11])
        #expect(PickerSample.box5.label == "5×5" && PickerSample.allCases.map(\.id).count == 4)
        let suite = testSuiteName()
        let s = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(s.pickerSample == .box5, "5×5 by default")
        s.pickerSample = .point
        #expect(AppSettings(defaults: UserDefaults(suiteName: suite)!).pickerSample == .point, "persisted")
        #expect(ViewerModel.pickFailure(MockError.invalid("the picked spot is black or clipped")).hasPrefix("Clipped or black"))
    }
}

/// The loupe through the app with a test renderer (fast, deterministic).
@MainActor @Suite(.serialized) struct PickerLoupeModelTests {
    static func app() async -> (AppModel, ViewerSizedRenderer) {
        let r = ViewerSizedRenderer()
        let m = makeViewerApp(renderer: r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        _ = await waitUntil { m.viewer.frame != nil && m.viewer.layout.main != nil }
        return (m, r)
    }

    /// Only the eyedroppers show it; it renders a small 1:1 region at Interactive
    /// priority, follows the cursor without new renders inside it, and hides on Esc.
    @Test func showsForEyedroppersRendersOneToOneAndHidesOnEsc() async throws {
        let (m, r) = await Self.app()
        let v = m.viewer, loupe = v.pickerLoupe
        let img = try #require(v.layout.main?.image)
        let p = CGPoint(x: img.midX, y: img.midY)
        v.hover(at: p)
        #expect(!loupe.shown, "not for the pan tool")
        m.wbPickerActive = true
        v.hover(at: p)
        #expect(loupe.shown && loupe.kind == .whiteBalance)
        #expect(await waitUntil { loupe.region != nil && loupe.readout != nil })
        let req = try #require(r.requests.last { $0.region != nil && $0.scale == 1 })
        #expect(req.priority == .interactive && req.format.isEightBit)
        #expect(Int((req.region!.width * Double(m.focusedPhoto!.meta.pixelWidth)).rounded()) <= 128, "a small region")
        let rendered = r.requests.count
        v.hover(at: CGPoint(x: p.x + 1, y: p.y + 1))
        try await Task.sleep(for: .milliseconds(50))
        #expect(r.requests.count == rendered, "a move inside the region needs no render")
        #expect(loupe.readout?.whiteBalance != nil, "the demo path neutralizes the loupe's own pixels")
        // The tool changing shows/hides it without a mouse move.
        m.perform(.escape)
        #expect(!m.wbPickerActive && !loupe.shown)
        m.colorTools.mode = .pickSkin
        #expect(loupe.shown && loupe.kind == .skinTone)
        m.colorTools.mode = .targeted(.hue)
        #expect(!loupe.shown, "no loupe for the targeted adjustment")
        m.colorTools.mode = .pickRange
        #expect(loupe.kind == .colorRange)
        m.colorTools.cancel()
        v.hover(at: nil)
        #expect(!loupe.shown)
    }

    /// Esc leaves the picker before it un-zooms; the edit is untouched.
    @Test func escLeavesThePickerFirst() async throws {
        let (m, _) = await Self.app()
        m.wbPickerActive = true
        m.viewer.zoom(to: .actual)
        let before = m.focusedEdit
        m.perform(.escape)
        #expect(!m.wbPickerActive && m.viewer.state.zoom == .actual && m.focusedEdit == before)
        m.perform(.escape)
        #expect(m.viewer.state.zoom == .fit)
    }

    /// The demo path: the click applies the readout's value, and stays on.
    @Test func clickAppliesTheReadoutAndStaysOn() async throws {
        let (m, _) = await Self.app()
        let v = m.viewer
        m.wbPickerActive = true
        let img = try #require(v.layout.main?.image)
        let p = CGPoint(x: img.minX + img.width * 0.3, y: img.minY + img.height * 0.6)
        v.hover(at: p)
        #expect(await waitUntil { v.pickerLoupe.readout?.whiteBalance != nil })
        let shown = try #require(v.pickerLoupe.readout?.whiteBalance)
        v.beginCanvasDrag(at: p)
        v.endCanvasDrag()
        #expect(m.focusedEdit.whiteBalance == shown && m.wbPickerActive)
    }
}

/// The pipeline path with a tools double: the readout shows the pipeline's value for the
/// sample size set, the click applies it, and a failed pick says why.
@MainActor @Suite(.serialized) struct PickerLoupePipelineTests {
    @Test func picksShowTheirValueFollowTheSampleSizeAndReportFailures() async throws {
        let m = makeViewerApp(renderer: Vw8GeometryRenderer())
        let tools = FakeDevelopTools()
        m.tools = tools
        m.toastDuration = .seconds(3600)
        await m.start()
        let v = m.viewer, loupe = v.pickerLoupe
        v.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        #expect(await waitUntil { v.frame?.mapping != nil && v.layout.main != nil })
        m.wbPickerActive = true
        let img = try #require(v.layout.main?.image)
        let p = CGPoint(x: img.minX + img.width * 0.25, y: img.minY + img.height * 0.5)
        v.hover(at: p)
        #expect(await waitUntil { loupe.readout?.whiteBalance != nil })
        #expect(tools.picks.last?.1 == 5, "5×5 by default")
        v.setPickerSample(.point)
        #expect(loupe.sample == .point && m.settings.pickerSample == .point)
        #expect(await waitUntil { tools.picks.last?.1 == 1 && loupe.readout?.whiteBalance != nil })
        let shown = try #require(loupe.readout?.whiteBalance)
        let picks = tools.picks.count
        v.beginCanvasDrag(at: p)
        v.endCanvasDrag()
        #expect(m.focusedEdit.whiteBalance == shown, "applied at once: the value on screen")
        #expect(tools.picks.count == picks, "not computed again")

        tools.fail = true
        let q = CGPoint(x: img.minX + img.width * 0.75, y: img.minY + img.height * 0.3)
        v.hover(at: q)
        #expect(await waitUntil { loupe.readout?.whiteBalanceError != nil }, "\(String(describing: loupe.readout))")
        let before = m.focusedEdit
        v.beginCanvasDrag(at: q)
        v.endCanvasDrag()
        #expect(m.focusedEdit == before && m.toast?.isError == true)
        // A click where nothing was measured yet computes it the same way.
        tools.fail = false
        let c = CGPoint(x: img.minX + img.width * 0.6, y: img.minY + img.height * 0.8)
        v.pickWhiteBalance(at: try #require(v.layout.imagePoint(at: c)))
        #expect(await waitUntil { m.focusedEdit != before })
        #expect(m.focusedEdit.whiteBalance.tint == 7 && !m.focusedEdit.whiteBalance.asShot)
        // The colour pickers take the loupe's sample.
        m.wbPickerActive = false
        m.colorTools.mode = .pickRange
        v.hover(at: c)
        #expect(await waitUntil { loupe.region?.edit == m.focusedEdit && loupe.readout != nil })
        #expect(v.loupeSample(display: try #require(v.layout.imagePoint(at: c))) != nil)
        // Missing originals get no loupe.
        m.noteOriginal(m.focusedID!, missing: true)
        v.hover(at: c)
        #expect(!loupe.shown)
    }
}

/// Hover starts one pick when the cursor rests, not one per pixel; the loupe and the
/// pick use the same (stored) edit, also in before/after and while a style is previewed;
/// and there's no loupe before the developed frame's mapping exists.
@MainActor @Suite(.serialized) struct PickerLoupeHoverTests {
    @Test func picksWaitForTheCursorToRestShareTheEditAndWaitForTheMapping() async throws {
        let m = makeViewerApp(renderer: Vw8GeometryRenderer())
        let tools = FakeDevelopTools()
        m.tools = tools
        await m.start()
        let v = m.viewer, loupe = v.pickerLoupe
        v.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        #expect(await waitUntil { v.frame?.mapping != nil && v.layout.main != nil })
        m.wbPickerActive = true
        let img = try #require(v.layout.main?.image)
        let picks = tools.picks.count
        for i in 0..<12 { v.hover(at: CGPoint(x: img.minX + img.width * (0.1 + 0.06 * Double(i)), y: img.midY)) }
        #expect(await waitUntil { loupe.readout?.whiteBalance != nil })
        try await Task.sleep(for: .milliseconds(200))
        #expect(tools.picks.count - picks == 1, "one pick where the cursor rested, not \(tools.picks.count - picks)")

        // A previewed style and before/after: the loupe's pixels and the pick both use the
        // stored edit (what a click changes).
        var styled = m.focusedEdit
        styled.exposure.ev = 1
        v.setPreviewEdit(styled)
        v.toggleBeforeAfter()
        let q = CGPoint(x: img.minX + img.width * 0.4, y: img.minY + img.height * 0.3)
        v.hover(at: q)
        #expect(await waitUntil { loupe.region != nil && v.loupeJobs.picks.last?.key.edit == m.focusedEdit })
        #expect(loupe.region?.edit == m.focusedEdit && loupe.region?.edit != styled)
        v.toggleBeforeAfter()
        v.setPreviewEdit(nil)

        // A photo far away (not prefetched): no loupe until its mapping lands, then it
        // shows by itself.
        m.focus(m.visibleIDs.last, extend: false)
        v.hover(at: q)
        #expect(v.mapping == nil && !loupe.shown)
        #expect(await waitUntil { v.mapping != nil && loupe.shown })
    }
}

/// The loupe on real A9 samples through ds-pipeline (skipped without samples).
@MainActor @Suite(.serialized) struct PickerLoupeLiveTests {
    static func live() async throws -> Vw8Live? {
        guard let live = try await Vw8Live.make(limit: 1) else { return nil }
        live.app.tools = RustDevelopTools(thumbnailCache: nil)
        live.app.wbPickerActive = true
        _ = await live.settled()
        return live
    }

    /// RGB of the region pixel holding frame-space pixel `p`.
    static func pixel(_ r: PickerLoupe.Region, _ p: LoupePixel) -> SIMD3<Int>? {
        guard let (x, y) = r.local(p) else { return nil }
        return r.frame.withBytes { b in
            let i = y * r.frame.bytesPerRow + x * 4
            return SIMD3(Int(b[i]), Int(b[i + 1]), Int(b[i + 2]))
        }
    }

    /// Hover at `p`: the loupe's pixel is the final-frame pixel under the cursor, and its
    /// region holds exactly that pixel of the full-resolution develop (checked against
    /// an independent 1:1 render of the 9×9 pixels around it).
    static func checkLoupe(_ live: Vw8Live, at p: CGPoint, _ label: String) async throws {
        let v = live.app.viewer, loupe = v.pickerLoupe
        let clock = ContinuousClock(), t = clock.now
        v.hover(at: p)
        let u = try #require(v.layout.imagePoint(at: p), "\(label): over the image")
        let space = try #require(v.mapping?.nativeSize, "\(label): developed display")
        let want = LoupePixel(x: Int(u.x * space.width), y: Int(u.y * space.height))
        #expect(loupe.pixel == want, "\(label): \(String(describing: loupe.pixel)) vs \(want)")
        #expect(await waitUntil(timeout: .seconds(30)) {
            loupe.region.map { $0.space == space && $0.local(want) != nil && $0.edit == live.app.focusedEdit } ?? false
        }, "\(label): region")
        let landed = clock.now - t
        let region = try #require(loupe.region)
        #expect(abs(region.rect.width * space.width - Double(region.frame.width)) < 1.01, "\(label): 1:1")
        let photo = try #require(live.app.focusedPhoto)
        let rect = CGRect(x: Double(want.x - 4) / space.width, y: Double(want.y - 4) / space.height,
                          width: 9 / space.width, height: 9 / space.height)
        let ref = try await live.develop.render(photo, edit: live.app.focusedEdit, request: RenderRequest(
            maxPixelSize: Int(max(space.width, space.height)), scale: 1, region: rect, format: region.frame.format,
            quality: .full, priority: .interactive, geometry: .final))
        let refRegion = PickerLoupe.Region(frame: ref, rect: try #require(ref.region), space: space, photo: photo.id,
                                           edit: live.app.focusedEdit)
        let a = try #require(Self.pixel(region, want)), b = try #require(Self.pixel(refRegion, want))
        #expect(abs(a.x - b.x) <= 1 && abs(a.y - b.y) <= 1 && abs(a.z - b.z) <= 1, "\(label): \(a) vs \(b)")
        print("LOUPE \(label): pixel \(want) of \(Int(space.width))×\(Int(space.height)) = \(a); region "
              + "\(region.frame.width)×\(region.frame.height) landed in \(landed)")
    }

    @Test func samplesTheFullResolutionPixelsAtFitOneToOneCroppedAndRotated() async throws {
        guard let live = try await Self.live() else { return }
        defer { live.cleanup() }
        let v = live.app.viewer
        var img = try #require(v.layout.main?.image)
        try await Self.checkLoupe(live, at: CGPoint(x: img.minX + img.width * 0.37, y: img.minY + img.height * 0.61), "fit")

        v.zoom(to: .actual, anchor: CGPoint(x: img.midX, y: img.midY))
        let pane = try #require(v.layout.main)
        try await Self.checkLoupe(live, at: CGPoint(x: pane.frame.midX + 37, y: pane.frame.midY - 23), "1:1")
        v.zoom(to: .fit)

        let old = v.frame
        let portrait = try #require(v.mapping.map { $0.nativeSize.height > $0.nativeSize.width })
        live.app.updateEdit(label: "Crop") { e in
            e.crop.rect = CGRect(x: 0.12, y: 0.08, width: 0.7, height: 0.75)
            e.crop.angle = 4
            e.crop.quarterTurns = 1
        }
        live.app.endEditGesture()
        _ = await live.settled(after: old)
        #expect(v.mapping.map { $0.nativeSize.height > $0.nativeSize.width } == !portrait, "a quarter turn")
        img = try #require(v.layout.main?.image)
        try await Self.checkLoupe(live, at: CGPoint(x: img.minX + img.width * 0.7, y: img.minY + img.height * 0.2),
                                  "cropped+rotated")

        // EDR screens: the viewer renders 16F, the loupe stays display-referred 8-bit.
        v.edrHeadroom = 2
        _ = await live.settled(after: v.frame)
        try await Self.checkLoupe(live, at: CGPoint(x: img.minX + img.width * 0.5, y: img.minY + img.height * 0.5), "EDR")
        #expect(v.pickerLoupe.region?.frame.format.isEightBit == true)
    }

    /// The Kelvin/tint the readout shows is exactly what the click applies (also for a
    /// click before the readout is ready), the picked spot then reads neutral, and the
    /// picker stays on.
    @Test func readoutEqualsTheAppliedWhiteBalance() async throws {
        guard let live = try await Self.live() else { return }
        defer { live.cleanup() }
        let m = live.app, v = m.viewer, loupe = v.pickerLoupe
        let id = try #require(m.focusedID)
        let img = try #require(v.layout.main?.image)
        let p = CGPoint(x: img.minX + img.width * 0.5, y: img.minY + img.height * 0.35)
        let clock = ContinuousClock(), t = clock.now
        v.hover(at: p)
        #expect(await waitUntil(timeout: .seconds(30)) { loupe.readout?.whiteBalance != nil })
        let readoutTime = clock.now - t
        let shown = try #require(loupe.readout?.whiteBalance)
        v.beginCanvasDrag(at: p) // the tap gesture
        v.endCanvasDrag()
        #expect(await waitUntil { !m.edit(for: id).whiteBalance.asShot })
        #expect(m.edit(for: id).whiteBalance == shown, "\(m.edit(for: id).whiteBalance) vs \(shown)")
        #expect(m.wbPickerActive, "stays on for another try")
        #expect(m.history.undoStack.last?.label == "White Balance Picker")
        // The spot reads neutral with the picked white balance.
        #expect(await waitUntil(timeout: .seconds(30)) {
            loupe.readout?.whiteBalance != nil && loupe.region?.edit == m.edit(for: id)
        })
        let after = try #require(loupe.readout)
        #expect(after.neutrality == "Neutral" || after.neutrality.hasPrefix("Nearly neutral"), "\(after)")
        #expect(loupe.announcement?.contains("Kelvin") == true, "VoiceOver gets the settled readout")
        print("LOUPE readout ready in \(readoutTime): \(Int(shown.temperature)) K tint \(shown.tint), after: \(after.neutrality)")

        // A click before the readout: the same computation, so the same value.
        let q = CGPoint(x: img.minX + img.width * 0.2, y: img.minY + img.height * 0.7)
        let u = try #require(v.layout.imagePoint(at: q))
        let space = try #require(v.mapping?.nativeSize)
        let px = (x: Int(u.x * space.width), y: Int(u.y * space.height))
        let centre = CGPoint(x: (Double(px.x) + 0.5) / space.width, y: (Double(px.y) + 0.5) / space.height)
        let edit = m.edit(for: id)
        let expected = WhiteBalanceEdit.custom(try await RustDevelopTools(thumbnailCache: nil)
            .pickWhiteBalance(for: m.focusedPhoto!, edit: edit, at: centre, sample: m.settings.pickerSample.pixels))
        v.pickWhiteBalance(at: u)
        #expect(await waitUntil(timeout: .seconds(30)) { m.edit(for: id).whiteBalance != edit.whiteBalance })
        #expect(m.edit(for: id).whiteBalance == expected)
    }

    /// Blown highlights are flagged; a mid-tone isn't.
    @Test func clippedSpotsAreFlagged() async throws {
        guard let live = try await Self.live() else { return }
        defer { live.cleanup() }
        let m = live.app, v = m.viewer, loupe = v.pickerLoupe
        let img = try #require(v.layout.main?.image)
        let p = CGPoint(x: img.minX + img.width * 0.5, y: img.minY + img.height * 0.5)
        v.hover(at: p)
        #expect(await waitUntil(timeout: .seconds(30)) { loupe.readout != nil })
        let normal = try #require(loupe.readout)
        _ = await live.edit("Exposure") { $0.exposure.ev = 5 }
        v.hover(at: p)
        #expect(await waitUntil(timeout: .seconds(30)) { loupe.region?.edit.exposure.ev == 5 && loupe.readout != nil })
        let blown = try #require(loupe.readout)
        #expect(blown.clipped, "+5 EV: \(blown)")
        #expect(!normal.clipped || normal.rgb.x >= 250 || normal.rgb.y >= 250 || normal.rgb.z >= 250, "\(normal)")
    }

    /// The colour-editor and skin-tone eyedroppers use the same loupe, and a click takes
    /// the colour it shows.
    @Test func colourPickersUseTheLoupe() async throws {
        guard let live = try await Self.live() else { return }
        defer { live.cleanup() }
        let m = live.app, v = m.viewer, loupe = v.pickerLoupe
        m.wbPickerActive = false
        m.colorTools.mode = .pickSkin
        let img = try #require(v.layout.main?.image)
        let p = CGPoint(x: img.minX + img.width * 0.45, y: img.minY + img.height * 0.55)
        v.hover(at: p)
        #expect(loupe.kind == .skinTone)
        #expect(await waitUntil(timeout: .seconds(30)) { loupe.readout != nil && loupe.region?.edit == m.focusedEdit })
        let u = try #require(v.layout.imagePoint(at: p))
        let rgb = try #require(v.loupeSample(display: u))
        let before = m.focusedEdit.colorEditor.skin
        v.beginCanvasDrag(at: p)
        v.endCanvasDrag()
        #expect(m.focusedEdit.colorEditor.skin == ColorPick.skin(for: rgb, keeping: before))
        #expect(m.colorTools.mode == nil && !loupe.shown, "one-shot, as before")
    }

    /// Following the cursor costs the main thread little (no stalls at display rate).
    @Test func hoverIsCheapOnTheMainThread() async throws {
        guard let live = try await Self.live() else { return }
        defer { live.cleanup() }
        let v = live.app.viewer, loupe = v.pickerLoupe
        let img = try #require(v.layout.main?.image)
        let c = CGPoint(x: img.midX, y: img.midY)
        v.hover(at: c)
        #expect(await waitUntil(timeout: .seconds(30)) { loupe.region != nil && loupe.readout?.whiteBalance != nil })
        var worst = Duration.zero
        for i in 0..<240 {
            let p = CGPoint(x: c.x + Double(i % 7) - 3, y: c.y + Double(i % 5) - 2)
            worst = max(worst, TestTiming.cpu { v.hover(at: p) })
        }
        print("LOUPE hover: worst main-thread CPU \(worst) over 240 moves")
        #expect(worst < .milliseconds(4), "\(worst)")
    }
}
