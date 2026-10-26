import CoreGraphics
import LuxtableAI
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Develop tools double: records what the app asks for.
final class FakeDevelopTools: DevelopTools, @unchecked Sendable {
    private let lock = NSLock()
    private var _thumbs: [(PhotoID, EditState)] = []
    private var _infos = 0
    var fail = false
    let asShot = WhiteBalanceValue(kelvin: 5200, tint: 4)

    var thumbs: [(PhotoID, EditState)] { lock.withLock { _thumbs } }
    var infos: Int { lock.withLock { _infos } }

    func info(for photo: Photo) async throws -> PhotoDevelopInfo {
        if fail { throw DevelopToolsError.unavailable("fake") }
        lock.withLock { _infos += 1 }
        return PhotoDevelopInfo(asShot: asShot, lens: photo.meta.lens, lensProfile: "Sony FE 85mm F1.4 GM",
                                iso: photo.meta.iso, renderer: "Fake GPU")
    }

    func autoWhiteBalance(for photo: Photo) async throws -> WhiteBalanceValue {
        if fail { throw DevelopToolsError.unavailable("fake") }
        return WhiteBalanceValue(kelvin: 6100, tint: -3)
    }

    func autoTone(for photo: Photo, edit: EditState) async throws -> EditState {
        var e = edit
        e.exposure.ev = 0.7
        e.hdr.shadows = 12
        e.levels.inWhite = 0.95
        return edit.withTone(from: e)
    }

    private var _picks: [(CGPoint, Int)] = []
    var picks: [(CGPoint, Int)] { lock.withLock { _picks } }

    func pickWhiteBalance(for photo: Photo, edit: EditState, at point: CGPoint, sample: Int) async throws -> WhiteBalanceValue {
        if fail { throw DevelopToolsError.unavailable("fake") }
        lock.withLock { _picks.append((point, sample)) }
        return WhiteBalanceValue(kelvin: 4800 + point.x * 100, tint: 7)
    }

    func editedThumbnail(for photo: Photo, edit: EditState) async throws -> (image: CGImage, cached: Bool) {
        if fail { throw DevelopToolsError.unavailable("fake") }
        lock.withLock { _thumbs.append((photo.id, edit)) }
        let ctx = CGContext(data: nil, width: 8, height: 8, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        return (ctx.makeImage()!, false)
    }
}

@MainActor @Suite struct Tl8ToolsModelTests {
    func model(_ tools: FakeDevelopTools = FakeDevelopTools()) async -> (AppModel, FakeDevelopTools) {
        let m = makeModel(counts: [12])
        m.tools = tools
        await m.start()
        return (m, tools)
    }

    @Test func autoAdjustTakesWhiteBalanceAndToneFromThePipeline() async {
        let (m, _) = await model()
        await m.autoAdjust()
        let e = m.focusedEdit
        #expect(!e.whiteBalance.asShot && e.whiteBalance.temperature == 6100 && e.whiteBalance.tint == -3)
        #expect(e.exposure.ev == 0.7 && e.hdr.shadows == 12 && e.levels.inWhite == 0.95)
        // Without tools (demo) the preview estimate stays.
        let demo = makeModel(counts: [3])
        await demo.start()
        await demo.autoAdjust()
        #expect(demo.focusedEdit.whiteBalance.temperature != 6100)
    }

    @Test func autoAdjustFailureIsShownAndChangesNothing() async {
        let f = FakeDevelopTools()
        f.fail = true
        let (m, _) = await model(f)
        let before = m.focusedEdit
        await m.autoAdjust()
        #expect(m.focusedEdit == before && m.toast?.isError == true)
    }

    @Test func pickerNeutralizesThroughThePipeline() async throws {
        let (m, _) = await model()
        let id = try #require(m.focusedID)
        await m.pickWhiteBalance(id: id, finalPoint: CGPoint(x: 0.5, y: 0.5))?.value
        let wb = m.edit(for: id).whiteBalance
        #expect(!wb.asShot && wb.temperature == 4850 && wb.tint == 7)
        #expect(m.history.undoStack.last?.label == "White Balance Picker")
        // No tools: nothing to do.
        let demo = makeModel(counts: [3])
        await demo.start()
        #expect(demo.pickWhiteBalance(id: demo.focusedID!, finalPoint: .zero) == nil)
        // A failure is reported.
        let f = FakeDevelopTools()
        f.fail = true
        let (m2, _) = await model(f)
        await m2.pickWhiteBalance(id: m2.focusedID!, finalPoint: .zero)?.value
        #expect(m2.toast?.isError == true && m2.focusedEdit.whiteBalance.asShot)
    }

    @Test func developInfoIsLoadedOncePerPhoto() async throws {
        let (m, tools) = await model()
        let ids = Array(m.visibleIDs.prefix(3))
        await m.loadDevelopInfo(for: ids)
        await m.loadDevelopInfo(for: ids)
        #expect(tools.infos == 3)
        #expect(m.asShot(for: ids[0]) == WhiteBalanceValue(kelvin: 5200, tint: 4))
        #expect(m.developInfo[ids[0]]?.iso == m.photo(ids[0])?.meta.iso, "ISO for noise reduction comes from the metadata")
    }

    /// Any paste re-renders what's on screen: every target's thumbnail key changes and the
    /// next load develops it with the pasted edit.
    @Test func pasteReRendersTheVisibleTargets() async throws {
        let (m, tools) = await model()
        let targets = Array(m.visibleIDs.prefix(4))
        let loaders = targets.map { _ in ThumbnailLoader(cache: m.thumbnails) }
        for (l, id) in zip(loaders, targets) { await l.load(m.photo(id)!, bucket: 256) }
        let keysBefore = zip(loaders, targets).map { $0.0.key(for: m.photo($0.1)!, bucket: 256) }
        #expect(tools.thumbs.isEmpty, "unedited photos keep the embedded preview")
        #expect(keysBefore.allSatisfy { $0.edit == 0 })
        m.focus(targets[0], extend: false)
        m.updateEdit(label: "ev") { $0.exposure.ev = 1.2 }
        m.copyAdjustments()
        let n = m.applyClipboard(to: targets, options: PasteOptions(groups: [.exposure]))
        #expect(n == 3, "the source already had it")
        for (l, id) in zip(loaders, targets) {
            let k = l.key(for: m.photo(id)!, bucket: 256)
            #expect(k.edit != 0, "the key follows the edit")
            await l.load(m.photo(id)!, bucket: 256)
        }
        #expect(Set(tools.thumbs.map(\.0)) == Set(targets))
        #expect(tools.thumbs.allSatisfy { $0.1.exposure.ev == 1.2 })
        #expect(m.thumbnails.developedLoads == 4)
        // Loaded again with the same edit: served from memory.
        let again = tools.thumbs.count
        for (l, id) in zip(loaders, targets) { await l.load(m.photo(id)!, bucket: 256) }
        #expect(tools.thumbs.count == again)
        // Undo: back to the embedded previews for the pasted targets.
        m.undo()
        #expect(targets.dropFirst().allSatisfy { m.thumbnails.fingerprint(m.photo($0)!) == 0 })
    }

    @Test func aFailedDevelopFallsBackToThePreview() async throws {
        let f = FakeDevelopTools()
        f.fail = true
        let (m, _) = await model(f)
        m.updateEdit(label: "ev") { $0.exposure.ev = 1 }
        let p = try #require(m.focusedPhoto)
        #expect(await m.thumbnails.image(for: p, bucket: 256) != nil)
        #expect(m.thumbnails.developedLoads == 1)
    }

    @Test func relativeWhiteBalancePasteUsesEachSidesAsShot() async throws {
        let (m, _) = await model()
        let ids = Array(m.visibleIDs.prefix(3))
        // Source: 500 K warmer and 2 more tint than its as-shot (5200 K, 4).
        m.focus(ids[0], extend: false)
        m.updateEdit(label: "wb") { $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 5700, tint: 6)) }
        m.copyAdjustments()
        // Target 1 is As Shot, target 2 is custom 4000 K.
        m.updateEdits([ids[2]], label: "wb") { _, e in e.whiteBalance = .custom(WhiteBalanceValue(kelvin: 4000, tint: -10)) }
        m.selection = Set(ids.dropFirst())
        m.settings.copyMode = .custom
        m.settings.copyGroups = [.whiteBalance]
        m.settings.copyRelativeGroups = [.whiteBalance]
        let now = m.applyToSelection()
        let n = now > 0 ? now : await m.pendingPaste?.value ?? 0
        #expect(n == 2)
        let a = m.edit(for: ids[1]).whiteBalance, b = m.edit(for: ids[2]).whiteBalance
        #expect(!a.asShot && abs(a.temperature - 5700) < 1 && abs(a.tint - 6) < 0.01, "as-shot target: its as-shot × ratio")
        #expect(abs(b.temperature - 4000 * 5700 / 5200) < 1 && abs(b.tint - (-8)) < 0.01)
    }

    @Test func readouts() {
        #expect(WhiteBalanceReadout.asShot(WhiteBalanceValue(kelvin: 5449.6, tint: 3.24)) == "As shot: \(Formatters.kelvin(5450)), tint +3.2")
        #expect(WhiteBalanceReadout.asShot(WhiteBalanceValue(kelvin: 5000, tint: 0.01)) == "As shot: \(Formatters.kelvin(5000)), tint 0")
        let info = PhotoDevelopInfo(asShot: .init(kelvin: 1, tint: 0), lens: "X", lensProfile: "Sony FE 24-70mm F2.8 GM", iso: 100, renderer: "")
        #expect(LensReadout.profile(info: info, exifLens: "FE 24-70mm F2.8 GM") == "Sony FE 24-70mm F2.8 GM")
        var none = info
        none.lensProfile = nil
        #expect(LensReadout.profile(info: none, exifLens: "Samyang 35") == "No profile for Samyang 35")
        #expect(LensReadout.profile(info: none, exifLens: "") == "No lens information")
        #expect(LensReadout.profile(info: nil, exifLens: "FE 50") == "FE 50")
        #expect(LensReadout.profile(info: nil, exifLens: nil) == "–")
    }
}

@Suite struct Tl8PasteAndIntentTests {
    @Test func relativeWhiteBalanceMirrorsDsCore() {
        let asShot = WhiteBalanceValue(kelvin: 5000, tint: 0)
        var src = WhiteBalanceEdit.custom(WhiteBalanceValue(kelvin: 6000, tint: 10))
        let tgt = WhiteBalanceEdit.custom(WhiteBalanceValue(kelvin: 4000, tint: -5))
        let out = AdjustmentClipboard.relativeWhiteBalance(source: src, target: tgt, sourceAsShot: asShot, targetAsShot: nil)
        #expect(out.temperature == 4800 && out.tint == 5)
        // Source unchanged (As Shot): the target keeps its own.
        src.asShot = true
        #expect(AdjustmentClipboard.relativeWhiteBalance(source: src, target: tgt, sourceAsShot: asShot, targetAsShot: nil) == tgt)
        // Missing as-shot context: absolute fallback.
        let s2 = WhiteBalanceEdit.custom(WhiteBalanceValue(kelvin: 6000, tint: 10))
        #expect(AdjustmentClipboard.relativeWhiteBalance(source: s2, target: tgt, sourceAsShot: nil, targetAsShot: nil) == s2)
        var asShotTarget = WhiteBalanceEdit()
        asShotTarget.asShot = true
        #expect(AdjustmentClipboard.relativeWhiteBalance(source: s2, target: asShotTarget, sourceAsShot: asShot, targetAsShot: nil) == s2)
        // Clamped.
        let hot = AdjustmentClipboard.relativeWhiteBalance(source: .custom(.init(kelvin: 50000, tint: 150)),
                                                           target: .custom(.init(kelvin: 40000, tint: 140)),
                                                           sourceAsShot: .init(kelvin: 2000, tint: -150), targetAsShot: nil)
        #expect(hot.temperature == 50000 && hot.tint == 150)
    }

    /// C2: "warmer" on an As Shot photo starts from the camera's as-shot K/tint.
    @Test func instructionsStartFromTheRealAsShot() async throws {
        let asShot = WhiteBalanceValue(kelvin: 4300, tint: 9)
        let e = EditState().applying(EditIntent(temperatureShift: 500), asShot: asShot)
        #expect(!e.whiteBalance.asShot && e.whiteBalance.temperature == 4800 && e.whiteBalance.tint == 9)
        let neutral = EditState().applying(EditIntent(temperatureShift: 500))
        #expect(neutral.whiteBalance.temperature == AutoAdjust.neutralKelvin + 500)
        let service = VisionAIService(language: RulesEditService())
        let warmer = try await service.applyInstruction("warmer", to: EditState(), asShot: asShot)
        #expect(!warmer.whiteBalance.asShot && warmer.whiteBalance.temperature > 4300 && warmer.whiteBalance.tint == 9)
        // The protocol default (services that don't know as-shot) ignores it.
        let mock = try await MockAIService().applyInstruction("warmer", to: EditState(), asShot: asShot)
        #expect(mock == (try await MockAIService().applyInstruction("warmer", to: EditState())))
    }

    @Test func smallPieces() {
        #expect(DevelopToolsError.unavailable("x").errorDescription == "Not available: x")
        #expect(ThumbnailJPEG.image(Data([1, 2, 3])) == nil)
        let wb = WhiteBalanceEdit.custom(WhiteBalanceValue(kelvin: 5123.4, tint: 2.345))
        #expect(!wb.asShot && wb.temperature == 5123 && wb.tint == 2.3)
        var a = EditState()
        a.crop.angle = 3
        var b = EditState()
        b.exposure.ev = 1
        b.crop.angle = 9
        let t = a.withTone(from: b)
        #expect(t.exposure.ev == 1 && t.crop.angle == 3, "tone only")
    }

    @Test func keystoneRoundTripsThroughTheSidecar() throws {
        var e = EditState()
        e.lens.keystoneVertical = 7.5
        e.lens.keystoneHorizontal = -3
        let (back, _) = try AdjustmentsCodec.decode(AdjustmentsCodec.encode(e))
        #expect(back.lens.keystoneVertical == 7.5 && back.lens.keystoneHorizontal == -3)
        #expect(EditState().lens.keystoneVertical == 0 && Specs.keystoneVertical.range == -30...30)
        #expect(EditCodec.decode(EditCodec.encode(e)).lens.keystoneHorizontal == -3)
    }
}

/// The real FFI tools on the real samples.
@Suite(.serialized) struct Tl8LiveToolsTests {
    func photo(_ url: URL) async throws -> Photo { try await RustRaw.photo(at: url, id: PhotoID(1)) }

    @Test func liveToolsOnARealSample() async throws {
        guard let url = sampleARWs().first else { return }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tl8-thumbs-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        let tools = RustDevelopTools(thumbnailCache: dir)
        let p = try await photo(url)
        let clock = ContinuousClock()
        var t = clock.now
        let info = try await tools.info(for: p)
        #expect((2500...9000).contains(info.asShot.kelvin) && !info.renderer.isEmpty)
        #expect(info.iso == p.meta.iso && info.lens == p.meta.lens, "ISO/lens for NR and the lens profile come from the file")
        #expect(info.lensProfile.map { $0.contains("FE") || $0.contains("Sony") } ?? true, "\(info.lensProfile ?? "nil")")
        let wb = try await tools.autoWhiteBalance(for: p)
        let wbTime = clock.now - t
        #expect((2000...15000).contains(wb.kelvin))
        t = clock.now
        var e = EditState()
        e.whiteBalance = .custom(wb)
        let toned = try await tools.autoTone(for: p, edit: e)
        let toneTime = clock.now - t
        #expect(toned.whiteBalance == e.whiteBalance && abs(toned.exposure.ev) <= 5)
        let picked = try await tools.pickWhiteBalance(for: p, edit: EditState(), at: CGPoint(x: 0.5, y: 0.35), sample: 0)
        #expect((2000...50000).contains(picked.kelvin))
        // Edited thumbnail: a render, then the disk cache.
        var edited = EditState()
        edited.exposure.ev = 1.5
        t = clock.now
        let first = try await tools.editedThumbnail(for: p, edit: edited)
        let renderTime = clock.now - t
        t = clock.now
        let second = try await tools.editedThumbnail(for: p, edit: edited)
        let hitTime = clock.now - t
        #expect(!first.cached && second.cached && max(first.image.width, first.image.height) == 512)
        #expect(tools.diskCacheBytes > 0)
        // It shows the edit: brighter than the embedded preview's thumbnail at the same size.
        let embedded = try await RustPreviewProvider().preview(for: url, maxPixelSize: 512)
        #expect(Tl8Pixels.meanLuma(first.image) > Tl8Pixels.meanLuma(embedded) + 0.05)
        print("TL8 live: info+auto WB \(wbTime), auto tone \(toneTime), edited thumb render \(renderTime), cache hit \(hitTime)")
        // No file: every tool refuses.
        await #expect(throws: DevelopToolsError.self) { try await tools.info(for: samplePhoto()) }
    }
}

enum Tl8Pixels {
    static func meanLuma(_ img: CGImage) -> Double {
        let (w, h) = (img.width, img.height)
        var px = [UInt8](repeating: 0, count: w * h * 4)
        let ctx = CGContext(data: &px, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
        var sum = 0.0
        for i in stride(from: 0, to: px.count, by: 4) {
            sum += 0.2126 * Double(px[i]) + 0.7152 * Double(px[i + 1]) + 0.0722 * Double(px[i + 2])
        }
        return sum / Double(w * h) / 255
    }
}
