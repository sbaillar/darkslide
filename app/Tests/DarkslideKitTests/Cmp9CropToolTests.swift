import CoreGraphics
import Foundation
import ImageIO
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

// [cmp9] C2/C3/L5: the crop and straighten tools, aspect presets, keys.

private let full = CGRect(x: 0, y: 0, width: 1, height: 1)

private func pixelRatio(_ r: CGRect, _ size: CGSize) -> CGFloat {
    (r.width * size.width) / max(r.height * size.height, 1e-9)
}

private struct FailingHorizon: HorizonDetecting {
    func horizon(of photo: Photo, images: any ImageProvider) async throws -> HorizonReading? { throw MockError.invalid("no image") }
}

/// Serves one fixed image for every photo.
private struct OneImage: ImageProvider {
    let image: CGImage
    func thumbnail(for photo: Photo, maxPixelSize: Int) async throws -> CGImage { image }
}

/// A sky/ground picture whose horizon rises `degrees` to the right.
private func horizonImage(degrees: Double) -> CGImage {
    let (w, h) = (800, 600)
    let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
    ctx.setFillColor(CGColor(srgbRed: 0.5, green: 0.7, blue: 1, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    let t = degrees * .pi / 180
    let yl = Double(h) / 2 - tan(t) * Double(w) / 2, yr = Double(h) / 2 + tan(t) * Double(w) / 2 // CG is y-up
    ctx.setFillColor(CGColor(srgbRed: 0.2, green: 0.35, blue: 0.1, alpha: 1))
    ctx.move(to: .zero)
    ctx.addLine(to: CGPoint(x: 0, y: yl))
    ctx.addLine(to: CGPoint(x: Double(w), y: yr))
    ctx.addLine(to: CGPoint(x: w, y: 0))
    ctx.closePath()
    ctx.fillPath()
    return ctx.makeImage()!
}

/// A horizon detector with a fixed answer (the real one is Vision).
private struct FixedHorizon: HorizonDetecting {
    var reading: HorizonReading?
    func horizon(of photo: Photo, images: any ImageProvider) async throws -> HorizonReading? { reading }
}

@Suite struct Cmp9GeometryTests {
    @Test func presetsCarryAspectOrientationAndSwap() {
        #expect(CropPreset.chips.map(\.rawValue) == ["Original", "1:1", "4:5", "5:4", "3:2", "2:3", "16:9", "Free"])
        #expect(CropPreset.r4x5.aspect == .r4x5 && CropPreset.r4x5.portrait == true)
        #expect(CropPreset.r5x4.aspect == .r4x5 && CropPreset.r5x4.portrait == false)
        #expect(CropPreset.r2x3.aspect == .r3x2 && CropPreset.r2x3.portrait == true)
        for p in CropPreset.allCases { #expect(p.swapped.swapped == p && p.swapped.aspect == p.aspect) }
        #expect(CropPreset.r16x9.swapped == .r9x16 && CropPreset.square.swapped == .square)
        #expect(CropPreset.matching(.r4x5, portrait: true) == .r4x5 && CropPreset.matching(.r4x5, portrait: false) == .r5x4)
        #expect(CropPreset.matching(.r3x2, portrait: true) == .r2x3 && CropPreset.matching(.r16x9, portrait: true) == .r9x16)
        #expect(CropPreset.matching(.original, portrait: true) == .original && CropPreset.matching(.free, portrait: false) == .free)
        #expect(CropPreset.matching(.r1x1, portrait: false) == .square)
        #expect(CropPreset.r3x2.id == "3:2")
    }

    @Test func fitIsTheLargestCentredRectOfTheRatio() {
        let size = CGSize(width: 6000, height: 4000)
        let r45 = CropGeometry.fit(full, ratio: 4.0 / 5.0, size: size)
        #expect(abs(pixelRatio(r45, size) - 0.8) < 1e-9 && abs(r45.height - 1) < 1e-9 && abs(r45.midX - 0.5) < 1e-9)
        let inner = CGRect(x: 0.1, y: 0.2, width: 0.5, height: 0.5)
        let sq = CropGeometry.fit(inner, ratio: 1, size: size)
        #expect(abs(pixelRatio(sq, size) - 1) < 1e-9 && inner.insetBy(dx: -1e-9, dy: -1e-9).contains(sq))
        #expect(abs(sq.midX - inner.midX) < 1e-9 && abs(sq.midY - inner.midY) < 1e-9)
        #expect(CropGeometry.fit(inner, ratio: 0, size: size) == inner)
        #expect(CropGeometry.ratio(.free, box: full, size: size) == nil)
        #expect(CropGeometry.ratio(.original, box: inner, size: size) == 1.5)
        #expect(CropGeometry.ratio(.r4x5, box: full, size: size) == 0.8)
        #expect(CropGeometry.ratio(.r5x4, box: full, size: size) == 1.25)
        #expect(CropGeometry.isPortrait(CGRect(x: 0, y: 0, width: 0.5, height: 1), size: size))
    }

    @Test func swapTurnsTheBoxAndKeepsItInside() {
        let size = CGSize(width: 6000, height: 4000)
        // A 3:2 box as big as the frame becomes 2:3, shrunk to the frame's height.
        let s = CropGeometry.swapOrientation(full, size: size)
        #expect(abs(pixelRatio(s, size) - 2.0 / 3.0) < 1e-6 && full.insetBy(dx: -1e-9, dy: -1e-9).contains(s))
        #expect(abs(s.height - 1) < 1e-9)
        // A small box near the edge is moved back inside.
        let edge = CropGeometry.swapOrientation(CGRect(x: 0.8, y: 0.4, width: 0.2, height: 0.1), size: size)
        #expect(full.insetBy(dx: -1e-9, dy: -1e-9).contains(edge))
        #expect(CropGeometry.swapOrientation(.zero, size: size) == .zero)
    }

    /// L5: an edge drag with a lock next to the image edge keeps the ratio (it used to be clipped).
    @Test func aspectLockHoldsAtTheImageEdge() {
        let size = CGSize(width: 1000, height: 1000)
        var d = CanvasDrag(tool: .crop, start: CGPoint(x: 0.95, y: 0.5))
        d.cropHandle = .edge(2) // bottom edge, pulled down: the width wants to pass the right edge
        d.initialCrop = CGRect(x: 0.8, y: 0.3, width: 0.15, height: 0.15)
        d.current = CGPoint(x: 0.95, y: 0.9)
        let r = CanvasInteraction.cropRect(for: d, imageSize: size, aspect: (1, 1))
        #expect(abs(pixelRatio(r, size) - 1) < 1e-6, "\(r)")
        #expect(full.insetBy(dx: -1e-9, dy: -1e-9).contains(r))
        #expect(abs(r.minY - 0.3) < 1e-9, "the edge not dragged stays: \(r)")
        // The left edge dragged outward near the left side.
        d.cropHandle = .edge(3)
        d.initialCrop = CGRect(x: 0.05, y: 0.1, width: 0.2, height: 0.2)
        d.start = CGPoint(x: 0.05, y: 0.2)
        d.current = CGPoint(x: 0, y: 0.2)
        let l = CanvasInteraction.cropRect(for: d, imageSize: size, aspect: (3, 2), portrait: false)
        #expect(abs(pixelRatio(l, size) - 1.5) < 1e-6 && abs(l.maxX - 0.25) < 1e-9, "\(l)")
        // A top edge with a portrait lock.
        d.cropHandle = .edge(0)
        d.initialCrop = CGRect(x: 0.4, y: 0.4, width: 0.2, height: 0.25)
        d.start = CGPoint(x: 0.5, y: 0.4)
        d.current = CGPoint(x: 0.5, y: 0)
        let t = CanvasInteraction.cropRect(for: d, imageSize: size, aspect: (5, 4), portrait: true)
        #expect(abs(pixelRatio(t, size) - 0.8) < 1e-6 && abs(t.maxY - 0.65) < 1e-9, "\(t)")
        // A fixed orientation wins over the drag's shape.
        let fixed = CanvasInteraction.cropRect(from: .zero, to: CGPoint(x: 0.9, y: 0.3), imageSize: size, aspect: (5, 4), portrait: true)
        #expect(abs(pixelRatio(fixed, size) - 0.8) < 1e-6)
        let commit = CanvasInteraction.commit(CanvasDrag(tool: .crop, start: .zero).moved(to: CGPoint(x: 0.9, y: 0.3)),
                                              imageSize: size, cropAspect: (5, 4), cropPortrait: true)
        if case .crop(let c) = commit { #expect(abs(pixelRatio(c, size) - 0.8) < 1e-6) } else { Issue.record("\(commit)") }
    }
}

extension CanvasDrag {
    fileprivate func moved(to p: CGPoint) -> CanvasDrag {
        var d = self
        d.move(to: p)
        return d
    }
}

@Suite struct Cmp9KeyTests {
    @Test func defaultKeysHaveNoConflicts() {
        let b = KeyBindings.defaults
        let keys = Array(b.keys.values)
        #expect(Set(keys).count == keys.count, "one command per key")
        #expect(b.command(for: "c") == .tool(.crop))
        #expect(b.command(for: "r") == .tool(.straighten))
        #expect(b.command(for: "a") == nil, "A is left unbound")
        #expect(b.command(for: "i") == .tool(.colorPicker))
        #expect(b.command(for: "k") == .togglePinCompare)
        #expect(keys.allSatisfy { !KeyBindings.reserved.contains($0) })
        // ⇧A stays Auto Adjust; plain R straightens, plain A does nothing.
        #expect(KeyboardRouter.command(for: KeyInput("A", shift: true)) == .auto)
        #expect(KeyboardRouter.command(for: KeyInput("r")) == .tool(.straighten))
        #expect(KeyboardRouter.command(for: KeyInput("a")) == nil)
        // Menu ⌘-shortcuts don't collide with the plain keys (menus own ⌘).
        #expect(KeyboardRouter.command(for: KeyInput("c", command: true)) == nil)
        let sheet = KeyboardRouter.cheatSheet().first { $0.section == "Compose" }
        #expect(sheet?.rows.map(\.keys) == ["C", "R", "I", "W"], "unbound mask tools are left off the sheet")
        #expect(KeyboardRouter.cheatSheet().flatMap(\.rows).contains { $0.keys == "K" && $0.action == "Pin for compare" })
        for c in [BindableCommand.cropTool, .straightenTool, .colorPickerTool] { #expect(!c.title.isEmpty) }
    }

    /// User request: R straightens. Bindings saved before that move A to R; a choice
    /// made since (the saved version says so) is kept; R taken by the user keeps A.
    @Test func straightenMovesFromAToRInSavedBindings() {
        var old = KeyBindings.defaults.dictionary
        old[KeyBindings.versionKey] = nil // saved by an older build
        old[BindableCommand.straightenTool.rawValue] = "a"
        let migrated = KeyBindings(dictionary: old)
        #expect(migrated.key(for: .straightenTool) == "r" && migrated.command(for: "a") == nil)
        #expect(KeyboardRouter.command(for: KeyInput("r"), bindings: migrated) == .tool(.straighten))
        // Saved since: A is the user's choice.
        var chosen = migrated
        chosen.bind(.straightenTool, to: "a")
        #expect(KeyBindings(dictionary: chosen.dictionary).key(for: .straightenTool) == "a")
        // An old save with a custom straighten key keeps it.
        var custom = old
        custom[BindableCommand.straightenTool.rawValue] = "q"
        #expect(KeyBindings(dictionary: custom).key(for: .straightenTool) == "q")
        // An old save where the user had put R on another command: no clash, A stays.
        var taken = old
        taken[BindableCommand.pick.rawValue] = "r"
        let kept = KeyBindings(dictionary: taken)
        #expect(kept.key(for: .straightenTool) == "a" && kept.key(for: .pick) == "r")
        // Saved before the straighten tool existed: it gets R.
        var older = old
        older[BindableCommand.straightenTool.rawValue] = nil
        #expect(KeyBindings(dictionary: older).key(for: .straightenTool) == "r")
        #expect(KeyBindings.defaults.dictionary[KeyBindings.versionKey] == "2")
    }

    /// R conflicts with nothing: no other default, no menu key (⌘-only there), no
    /// survey key (the survey swallows it like any non-culling key).
    @MainActor @Test func rConflictsWithNothing() {
        let b = KeyBindings.defaults
        #expect(b.keys.filter { $0.value == "r" }.map(\.key) == [.straightenTool])
        for c in MenuCommand.allCases {
            if let s = c.shortcut { #expect(s.key != "r" || !s.modifiers.isEmpty, "\(c)") }
        }
        #expect(MenuCommand.allCases.allSatisfy { $0.shortcut.map { !$0.modifiers.isEmpty } ?? true }, "menus own ⌘ keys only")
        let s = SurveyModel()
        #expect(s.handle(KeyInput("r")) == nil, "inactive survey ignores it")
        _ = s.begin([PhotoID(1), PhotoID(2)])
        #expect(s.handle(KeyInput("r")) == .handled && s.isActive, "swallowed, no tool switch mid-survey")
    }

    @Test func savedBindingsMovePinCompareOffC() {
        // Saved by a build before the crop tool: pin-compare on its old default C.
        var old = KeyBindings.defaults.dictionary
        old[BindableCommand.cropTool.rawValue] = nil
        old[BindableCommand.straightenTool.rawValue] = nil
        old[BindableCommand.colorPickerTool.rawValue] = nil
        old[BindableCommand.pinCompare.rawValue] = "c"
        let migrated = KeyBindings(dictionary: old)
        #expect(migrated.key(for: .cropTool) == "c" && migrated.key(for: .pinCompare) == "k")
        // A user's own choice for pin-compare is kept.
        var own = old
        own[BindableCommand.pinCompare.rawValue] = "q"
        let kept = KeyBindings(dictionary: own)
        #expect(kept.key(for: .pinCompare) == "q" && kept.key(for: .cropTool) == "c")
        // After the crop tool exists, an explicit C for pin-compare is the user's.
        var now = KeyBindings.defaults.dictionary
        now[BindableCommand.pinCompare.rawValue] = "c"
        now[BindableCommand.cropTool.rawValue] = ""
        let explicit = KeyBindings(dictionary: now)
        #expect(explicit.key(for: .pinCompare) == "c" && explicit.key(for: .cropTool) == nil)
    }

    @MainActor @Test func toolMenuAndToolbarHelp() async {
        let m = makeViewerApp()
        await m.start()
        #expect(CommandCatalog.toolMenu == [.cropTool, .straightenTool, .colorPickerTool, .whiteBalanceTool,
                                            .brushTool, .eraserTool, .gradientTool, .radialTool])
        #expect(CommandCatalog.toolMenu.allSatisfy { CommandCatalog.isEnabled($0, model: m) })
        #expect(ToolbarHelp.tool(.crop, .defaults).contains("(C)"))
        #expect(ToolbarHelp.tool(.straighten, .defaults).contains("(R)"))
        #expect(ToolbarHelp.tool(.colorPicker, .defaults).contains("(I)"))
        #expect(ToolbarHelp.tool(.brush, .defaults) == "Brush")
        #expect(CommandCatalog.title(.cropTool, bindings: .defaults).contains("Crop"))
    }
}

@MainActor
@Suite struct Cmp9ToolFlowTests {
    func app() async -> AppModel {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        _ = await waitUntil { m.viewer.frame != nil }
        return m
    }

    func key(_ m: AppModel, _ k: KeyInput) -> Bool {
        m.handleKey(k, focus: .none, sheetOpen: false, isMainWindow: true)
    }

    @Test func cKeyDragEnterCommitsAndEscCancels() async throws {
        let m = await app()
        let id = try #require(m.focusedID)
        #expect(key(m, KeyInput("c")) && m.viewer.canvasTool == .crop)
        let img = try #require(m.viewer.layout.main?.image)
        // Drag the bottom-right corner in: a smaller crop.
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.maxX, y: img.maxY))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.midX, y: img.midY))
        #expect(m.viewer.cropDragPreview != nil)
        m.viewer.endCanvasDrag()
        let cropped = m.edit(for: id).crop.rect
        #expect(cropped.width < 0.6 && cropped.height < 0.6, "\(cropped)")
        #expect(key(m, KeyInput(special: .returnKey)))
        #expect(m.viewer.canvasTool == .pan && m.edit(for: id).crop.rect == cropped, "⏎ keeps the crop")
        // Again: Esc puts it back as it was when the tool started.
        #expect(key(m, KeyInput("c")) && m.viewer.canvasTool == .crop)
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.minX + 2, y: img.minY + 2))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.midX - 50, y: img.midY - 50))
        // Esc during the drag only drops the drag.
        #expect(key(m, KeyInput(special: .escape)) && m.viewer.activeDrag == nil && m.viewer.canvasTool == .crop)
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.minX + 2, y: img.minY + 2))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.midX - 50, y: img.midY - 50))
        m.viewer.endCanvasDrag()
        #expect(m.edit(for: id).crop.rect != cropped)
        #expect(key(m, KeyInput(special: .escape)))
        #expect(m.viewer.canvasTool == .pan && m.edit(for: id).crop.rect == cropped, "Esc restores")
        // C toggles the tool off again (keeping the edit), keys pass through outside the tools.
        #expect(key(m, KeyInput("c")) && key(m, KeyInput("c")) && m.viewer.canvasTool == .pan)
        #expect(!m.viewer.handleToolKey(KeyInput(special: .returnKey)))
    }

    @Test func xSwapsOrientationInsideTheCropToolOnly() async throws {
        let m = await app()
        let id = try #require(m.focusedID)
        m.setCropAspect(.r5x4)
        let size = m.viewer.cropBox(for: m.edit(for: id)).size
        #expect(m.viewer.cropPreset == .r5x4)
        #expect(key(m, KeyInput("c")))
        #expect(key(m, KeyInput("x")), "X in the crop tool swaps")
        let box = m.viewer.cropBox(for: m.edit(for: id))
        #expect(abs(pixelRatio(box.rect, size) - 0.8) < 1e-3 && m.viewer.cropPreset == .r4x5, "\(box.rect)")
        #expect(m.photo(id)?.pick == PickState.none, "X didn't reject")
        m.viewer.commitTool()
        // Outside the tool X rejects as bound.
        #expect(key(m, KeyInput("x")) && m.photo(id)?.pick == .rejected)
        // Free boxes swap too.
        m.setCropAspect(.free)
        m.swapCropOrientation()
        #expect(m.edit(for: id).crop.aspect == .free)
    }

    @Test func aspectChipsCropCentredInOneUndoStep() async throws {
        let m = await app()
        let id = try #require(m.focusedID)
        let size = m.viewer.cropBox(for: m.edit(for: id)).size
        let steps = m.history.undoStack.count
        m.setCropAspect(.r4x5)
        let box = m.viewer.cropBox(for: m.edit(for: id))
        #expect(abs(pixelRatio(box.rect, size) - 0.8) < 1e-3, "\(box.rect) in \(size)")
        #expect(abs(box.rect.midX - 0.5) < 1e-6 && abs(box.rect.midY - 0.5) < 1e-6)
        #expect(m.edit(for: id).crop.aspect == .r4x5 && m.history.undoStack.count == steps + 1)
        m.setCropAspect(.square)
        #expect(abs(pixelRatio(m.viewer.cropBox(for: m.edit(for: id)).rect, size) - 1) < 1e-3)
        m.setCropAspect(.original)
        #expect(m.edit(for: id).crop.rect == full && m.viewer.cropPreset == .original)
        // Original (the default) and Free drag freely; a ratio locks in the box's orientation.
        #expect(m.viewer.cropLock(for: m.edit(for: id)).aspect == nil)
        m.setCropAspect(.r2x3)
        let lock = m.viewer.cropLock(for: m.edit(for: id))
        #expect(lock.aspect.map { $0.long == 3 && $0.short == 2 } == true && lock.portrait == true)
        m.setCropAspect(.free)
        #expect(m.viewer.cropLock(for: m.edit(for: id)).aspect == nil)
        m.undo()
        #expect(m.edit(for: id).crop.aspect == .r3x2, "one undo step per chip")
    }

    @Test func autoStraightenLevelsOnlyConfidentHorizons() async throws {
        let m = await app()
        let id = try #require(m.focusedID)
        m.toastDuration = .seconds(60)
        m.horizonDetector = FixedHorizon(reading: HorizonReading(degrees: 2.4, confidence: 0.9))
        let a = await m.autoStraighten()
        #expect(a == -2.4 && m.edit(for: id).crop.angle == -2.4)
        #expect(m.toast?.message.contains("2.4°") == true, "\(String(describing: m.toast))")
        m.horizonDetector = FixedHorizon(reading: HorizonReading(degrees: 5, confidence: 0.2))
        #expect(await m.autoStraighten() == nil && m.edit(for: id).crop.angle == -2.4)
        #expect(m.toast?.message.contains("No clear horizon") == true)
        m.horizonDetector = FixedHorizon(reading: nil)
        #expect(await m.autoStraighten() == nil)
        m.horizonDetector = FixedHorizon(reading: HorizonReading(degrees: 30, confidence: 1))
        #expect(await m.autoStraighten() == nil, "implausible tilt")
    }

    /// The real detector (Vision) on a synthetic 5° horizon, and a detector error.
    @Test func visionHorizonDetectorAndErrors() async throws {
        let photo = samplePhoto(1)
        let r = try #require(try await VisionHorizonDetector().horizon(of: photo, images: OneImage(image: horizonImage(degrees: 5))))
        #expect(r.degrees > 3 && r.degrees < 7 && r.confidence > 0, "\(r)")
        let m = await app()
        m.toastDuration = .seconds(60)
        m.horizonDetector = FailingHorizon()
        #expect(await m.autoStraighten() == nil && m.toast?.isError == true)
    }

    @Test func straightenToolLevelsADraggedLine() async throws {
        let m = await app()
        let id = try #require(m.focusedID)
        #expect(key(m, KeyInput("r")) && m.viewer.canvasTool == .straighten)
        let img = try #require(m.viewer.layout.main?.image)
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.minX + 10, y: img.midY))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.maxX - 10, y: img.midY - 40))
        m.viewer.endCanvasDrag()
        let angle = m.edit(for: id).crop.angle
        #expect(angle < -1 && angle > -10, "a line rising to the right is levelled clockwise: \(angle)")
        // Esc cancels the straighten too.
        #expect(key(m, KeyInput(special: .escape)) && m.edit(for: id).crop.angle == 0 && m.viewer.canvasTool == .pan)
        // The color picker is reachable by key.
        #expect(key(m, KeyInput("i")) && m.viewer.canvasTool == .colorPicker)
        #expect(key(m, KeyInput("i")) && m.viewer.canvasTool == .pan)
    }

    @Test func toolsNeedAPhotoAndLeaveTheGrid() async throws {
        let m = await app()
        m.browserMode = .grid
        m.perform(.tool(.crop))
        #expect(m.browserMode == .filmstrip && m.viewer.canvasTool == .crop)
        m.viewer.cancelTool()
        #expect(m.viewer.canvasTool == .pan)
    }
}

// MARK: - Live: through the real pipeline render and export

@MainActor
@Suite(.serialized) struct Cmp9LiveTests {
    @Test func cropToolAspectAndStraightenReachTheRenderAndTheExport() async throws {
        guard !sampleARWs().isEmpty else { print("CMP9 live: no samples, skipped"); return }
        let live = try #require(try await Vw8Live.make(limit: 1))
        defer { live.cleanup() }
        let m = live.app
        let id = try #require(m.focusedID)
        let first = try #require(await live.settled())
        // 4:5: the developed frame is 4:5 (portrait) whatever the photo's orientation.
        let old = m.viewer.frame
        m.setCropAspect(.r4x5)
        let f45 = try #require(await live.settled(after: old))
        #expect(abs(Double(f45.width) / Double(f45.height) - 0.8) < 0.01, "\(f45.width)×\(f45.height) was \(first.width)×\(first.height)")
        // The crop tool: drag in from a corner, ⏎ applies; the frame follows the box.
        #expect(m.handleKey(KeyInput("c"), focus: .none, sheetOpen: false, isMainWindow: true))
        #expect(await waitUntil(timeout: .seconds(30)) { m.viewer.layout.main != nil && m.viewer.cropRect != full })
        let img = try #require(m.viewer.layout.main?.image)
        let box = m.viewer.cropRect
        let corner = CGPoint(x: img.minX + box.maxX * img.width, y: img.minY + box.maxY * img.height)
        m.viewer.beginCanvasDrag(at: corner)
        m.viewer.updateCanvasDrag(to: CGPoint(x: corner.x - img.width * box.width * 0.4, y: corner.y - img.height * box.height * 0.4))
        m.viewer.endCanvasDrag()
        let before = m.viewer.frame
        #expect(m.handleKey(KeyInput(special: .returnKey), focus: .none, sheetOpen: false, isMainWindow: true))
        let dragged = try #require(await live.settled(after: before))
        #expect(abs(Double(dragged.width) / Double(dragged.height) - 0.8) < 0.01, "the 4:5 lock held while dragging")
        // Straighten through the tool's line.
        #expect(m.handleKey(KeyInput("r"), focus: .none, sheetOpen: false, isMainWindow: true))
        let simg = try #require(m.viewer.layout.main?.image)
        m.viewer.beginCanvasDrag(at: CGPoint(x: simg.minX + 20, y: simg.midY))
        m.viewer.updateCanvasDrag(to: CGPoint(x: simg.maxX - 20, y: simg.midY - 30))
        m.viewer.endCanvasDrag()
        m.viewer.commitTool()
        let edit = m.edit(for: id)
        #expect(edit.crop.angle < -0.5)
        _ = try #require(await live.settled(after: dragged))
        // Export honours all of it: the full-size JPEG has the crop's ratio.
        let url = try #require(m.focusedPhoto?.url)
        let out = live.dir.appendingPathComponent("Output")
        let job = ExportJob(items: [ExportJobItem(photoID: id, url: url, seq: 1, edit: edit)],
                            recipes: [ExportRecipe(name: ExportRecipe.fullSizeJPEGName, format: .jpeg, quality: 85,
                                                   fileNameTemplate: "{seq}")],
                            defaultFolder: out)
        let o = try await RustExportService().export(job) { _, _ in }
        #expect(o.failures.isEmpty, "\(o.failures)")
        let w = try #require(o.written.first)
        #expect(abs(Double(w.width) / Double(w.height) - 0.8) < 0.01, "export \(w.width)×\(w.height)")
        let src = try #require(CGImageSourceCreateWithURL(w.url as CFURL, nil))
        print("CMP9 live: 4:5 frame \(f45.width)×\(f45.height), dragged \(dragged.width)×\(dragged.height), angle \(edit.crop.angle), export \(w.width)×\(w.height)")
        let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any]
        #expect(props?[kCGImagePropertyPixelWidth] as? Int == w.width)
    }
}
