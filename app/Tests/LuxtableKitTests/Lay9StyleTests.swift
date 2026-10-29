import CoreGraphics
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Styles library (M9): non-destructive application, the 20-style pack, the store's
/// update/duplicate/rename, persistence, and style thumbnails.
@Suite struct Lay9StyleTests {
    /// A photo with its own correction in every group a style might touch.
    static func corrected() -> EditState {
        var e = EditState()
        e.exposure.ev = 1
        e.exposure.contrast = 10
        e.exposure.saturation = 5
        e.presence.vibrance = 12
        e.hdr.shadows = 20
        e.detail.clarity = 7
        e.detail.sharpenAmount = 200
        e.colorBalance.midtones = WheelValue(hue: 100, saturation: 0.1, lightness: 0)
        e.colorEditor.bands[4].saturation = 30
        e.curve.points[.red] = [CurvePoint(0.5, 0.55)]
        return e
    }

    @Test func thePackHasTwentyStableStylesInKnownSections() {
        #expect(Preset.builtIns.count == 20)
        #expect(Set(Preset.builtIns.map(\.id)).count == 20 && Set(Preset.builtIns.map(\.name)).count == 20)
        #expect(Preset.builtIns.allSatisfy { $0.isBuiltIn && Preset.builtInCategories.contains($0.category ?? "") })
        #expect(Preset.builtInCategories.allSatisfy { c in Preset.builtIns.contains { $0.category == c } })
        #expect(Preset.builtIns.filter { $0.category == "Black & White" }.count >= 4)
        // Old ids keep working for anything that stored them.
        #expect(Preset.builtIns.contains { $0.id.uuidString == "00000000-0000-0000-0000-00000000A002" && $0.name == "Warm Film" })
    }

    @Test func noStyleResetsThePhotosOwnCorrection() {
        let base = Self.corrected()
        for style in Preset.builtIns {
            let out = style.apply(to: base)
            #expect(out.exposure.ev == (base.exposure.ev + style.edit.exposure.ev).clamped(-5, 5), "\(style.name) EV")
            if !style.absoluteFields.contains("exposure.saturation") {
                #expect(out.exposure.saturation == (base.exposure.saturation + style.edit.exposure.saturation).clamped(-100, 100),
                        "\(style.name) saturation adds")
            }
            #expect(out.presence == base.presence, "\(style.name) leaves presence")
            #expect(out.detail.sharpenAmount == 200, "\(style.name) keeps sharpening")
            #expect(out.curve.points[.red] == base.curve.points[.red], "\(style.name) keeps the red curve")
            if style.edit.colorBalance.midtones == WheelValue() {
                #expect(out.colorBalance.midtones == base.colorBalance.midtones, "\(style.name) keeps midtones")
            }
            if style.edit.colorEditor.bands[4] == ColorBandEdit() {
                #expect(out.colorEditor.bands[4] == base.colorEditor.bands[4], "\(style.name) keeps cyan")
            }
            #expect(out.layers == base.layers && out.crop == base.crop && out.whiteBalance == base.whiteBalance)
        }
    }

    @Test func blackAndWhiteIsExactAndStylesStack() {
        let base = Self.corrected()
        let bw = Preset.builtIns.first { $0.name == "B&W High Contrast" }!
        let out = bw.apply(to: base)
        #expect(out.exposure.saturation == -100, "exact, whatever the photo had")
        #expect(out.exposure.contrast == 45, "contrast adds: 10 + 35")
        #expect(out.curve.points[.luma] == bw.edit.curve.points[.luma])
        let punchy = Preset.builtIns.first { $0.name == "Punchy" }!
        let twice = Preset.apply([punchy, punchy], to: EditState())
        #expect(twice.exposure.contrast == 44 && twice.hdr.highlights == -40, "tone deltas stack")
        #expect(twice.detail.clarity == 20, "a field the style sets is set, not added")
        let warm = Preset.builtIns.first { $0.name == "Warm" }!
        let w = warm.apply(to: base)
        #expect(w.colorBalance.master.hue == 40 && w.colorBalance.midtones == base.colorBalance.midtones)
        #expect(Preset.apply([], to: base) == base)
    }

    @Test func userPresetsAreNonDestructiveAndScopedToTheirGroups() {
        var e = EditState()
        e.exposure.ev = 0.5
        e.hdr.highlights = -30
        e.levels.gamma = 1.4
        e.colorEditor.advanced = [ColorRangeEdit(centerHue: 200, saturation: 20)]
        e.colorEditor.skin = SkinToneEdit(hueUniformity: 40)
        e.layers = [LayerEdit(id: 0, name: "x")]
        e.spots = [SpotEdit()]
        let p = Preset(name: "Mine", groups: [.exposure, .hdr, .levels, .colorEditor, .layers, .spots],
                       relative: Preset.defaultRelative(for: [.exposure, .hdr]), edit: e)
        #expect(p.groups == [.exposure, .hdr, .levels, .colorEditor], "never per-image layers or spots")
        #expect(p.relative == [.exposure, .hdr])
        #expect(Preset.defaultRelative(for: [.whiteBalance, .exposure, .presence]) == [.exposure, .presence])
        let out = p.apply(to: Self.corrected())
        #expect(out.exposure.ev == 1.5 && out.hdr.highlights == -30 && out.hdr.shadows == 20)
        #expect(out.levels.gamma == 1.4 && out.colorEditor.advanced == e.colorEditor.advanced && out.colorEditor.skin == e.colorEditor.skin)
        #expect(out.layers.isEmpty && out.spots.isEmpty)
        #expect(!p.summary.isEmpty && p.summary.hasPrefix("Exposure"))
        // absoluteFields outside the preset's groups are dropped.
        let q = Preset(name: "q", groups: [.exposure], absoluteFields: ["exposure.ev", "detail.clarity", "nope"], edit: e)
        #expect(q.absoluteFields == ["exposure.ev"])
    }

    @Test func editCodecCarriesPresenceRangesAndSkin() throws {
        var e = EditState()
        e.presence = PresenceEdit(vibrance: 20, texture: -10, dehaze: 5)
        e.colorEditor.advanced = [ColorRangeEdit(centerHue: 120, width: 30, hueShift: 4)]
        e.colorEditor.skin = SkinToneEdit(hueUniformity: 30)
        let data = try JSONEncoder().encode(EditCodec.encode(e))
        let back = EditCodec.decode(try JSONDecoder().decode(EditSnapshot.self, from: data))
        #expect(back.presence == e.presence && back.colorEditor == e.colorEditor)
        #expect(EditCodec.group(of: "presence.vibrance") == .presence && EditCodec.group(of: "wb.tint") == .whiteBalance)
        #expect(EditCodec.group(of: "cb.master.hue") == .colorBalance && EditCodec.group(of: "zzz") == nil)
        for (k, _) in EditCodec.scalars { #expect(EditCodec.group(of: k) != nil, "\(k)") }
        for (k, _) in EditCodec.flags { #expect(EditCodec.group(of: k) != nil, "\(k)") }
    }

    func tempURL() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("lt-lay9-presets-\(UUID().uuidString)/presets.json")
    }

    @MainActor
    @Test func storeUpdatesDuplicatesRenamesAndPersistsExactFields() throws {
        let url = tempURL()
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        let store = PresetStore(fileURL: url)
        var e = EditState()
        e.exposure.contrast = 15
        let p = try #require(store.save(name: "Base", edit: e, groups: [.exposure]))
        #expect(p.relative == [.exposure], "saved presets add to each photo's exposure by default")
        e.exposure.contrast = 25
        store.update(p.id, from: e)
        #expect(store.userPresets.first?.edit.exposure.contrast == 25)
        let bw = Preset.builtIns.first { $0.name == "B&W Neutral" }!
        let copy = store.duplicate(bw)
        #expect(copy.name == "B&W Neutral Copy" && !copy.isBuiltIn && copy.absoluteFields == ["exposure.saturation"])
        store.rename(copy.id, to: "Base")
        #expect(store.userPresets.last?.name == "Base 2", "rename keeps names unique")
        store.rename(copy.id, to: "Base 2")
        #expect(store.styles(in: "Film").count == 5)
        let reloaded = PresetStore(fileURL: url)
        #expect(reloaded.userPresets.map(\.name) == ["Base", "Base 2"])
        #expect(reloaded.userPresets[1].absoluteFields == ["exposure.saturation"])
        #expect(reloaded.userPresets[1].apply(to: Self.corrected()).exposure.saturation == -100)
        store.update(UUID(), from: e) // unknown id: nothing happens
        #expect(store.userPresets.count == 2)
    }

    @MainActor
    @Test func versionOneFilesStillLoad() throws {
        let url = tempURL()
        defer { try? FileManager.default.removeItem(at: url.deletingLastPathComponent()) }
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let v1 = #"{"version":1,"presets":[{"id":"6E0C0A67-5D8E-4D3B-9C9E-1A2B3C4D5E6F","name":"Old","groups":["exposure"],"relative":[],"edit":{"values":{"exposure.ev":0.7},"curves":{}},"autoApplyOnImport":false}]}"#
        try Data(v1.utf8).write(to: url)
        let store = PresetStore(fileURL: url)
        #expect(store.userPresets.first?.name == "Old" && store.userPresets.first?.edit.exposure.ev == 0.7)
        #expect(store.userPresets.first?.absoluteFields.isEmpty == true && store.lastError == nil)
    }
}

/// Style thumbnails: each style applied to the focused photo's current edit.
@MainActor
@Suite struct Lay9StyleThumbnailTests {
    final class Recorder: @unchecked Sendable {
        let lock = NSLock()
        var edits: [EditState] = []
        func add(_ e: EditState) { lock.withLock { edits.append(e) } }
        var count: Int { lock.withLock { edits.count } }
    }

    nonisolated static func image() -> CGImage {
        let ctx = CGContext(data: nil, width: 4, height: 3, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        return ctx.makeImage()!
    }

    static func photo(_ n: Int64 = 1) -> Photo {
        var p = samplePhoto(n)
        p.url = URL(fileURLWithPath: "/tmp/x\(n).ARW")
        return p
    }

    @Test func rendersEachStyleOnTheCurrentEditAndStartsOverOnChange() async {
        let rec = Recorder()
        let t = StyleThumbnails(render: { _, e in
            rec.add(e)
            return Self.image()
        })
        #expect(t.isAvailable)
        let presets = Array(Preset.builtIns.prefix(3))
        var base = EditState()
        base.exposure.ev = 1
        t.refresh(photo: Self.photo(), edit: base, presets: presets)
        await t.settle()
        #expect(t.images.count == 4 && rec.count == 4, "three styles and the Original")
        #expect(rec.edits[0] == EditState(), "the Original tile first: the photo with no adjustments")
        #expect(rec.edits[1] == presets[0].apply(to: base), "tiles show the style on the photo's own edit")
        t.refresh(photo: Self.photo(), edit: base, presets: presets)
        await t.settle()
        #expect(rec.count == 4, "cached")
        t.invalidate(presets[1].id)
        t.refresh(photo: Self.photo(), edit: base, presets: presets)
        await t.settle()
        #expect(rec.count == 5)
        base.exposure.ev = 2
        t.refresh(photo: Self.photo(), edit: base, presets: presets)
        // A click on a style changes the edit: the tiles stay up (dimmed) until each new one lands.
        #expect(presets.allSatisfy { t.tile($0.id)?.isStale == true }, "no tile blanks while the new ones render")
        #expect(t.tile(StyleThumbnails.originalID)?.isStale == false, "the Original doesn't depend on the edit")
        await t.settle()
        #expect(rec.count == 8 && t.renders == 8, "a new edit re-renders every style tile, not the Original")
        #expect(t.stale.isEmpty && presets.allSatisfy { t.tile($0.id)?.isStale == false })
        // A look's tile on a photo that already has the look: what a click leaves, not
        // the look twice (Med 2).
        let cine = LookPack.look(id: "look.cinematic")!
        let withLook = cine.apply(to: base)
        t.refresh(photo: Self.photo(), edit: withLook, presets: [cine.preset])
        await t.settle()
        #expect(rec.edits.last == withLook)
        t.refresh(photo: nil, edit: base, presets: presets)
        #expect(t.images.isEmpty && t.stale.isEmpty)
        var video = Self.photo(2)
        video.url = URL(fileURLWithPath: "/tmp/clip.MP4")
        t.refresh(photo: video, edit: base, presets: presets)
        #expect(t.images.isEmpty)
        t.cancel()
    }

    @Test func anotherPhotosTilesNeverShowEvenDimmed() async {
        let t = StyleThumbnails(render: { _, _ in Self.image() })
        let presets = Array(Preset.builtIns.prefix(2))
        t.refresh(photo: Self.photo(1), edit: EditState(), presets: presets)
        await t.settle()
        t.refresh(photo: Self.photo(2), edit: EditState(), presets: presets)
        #expect(t.images.isEmpty && t.stale.isEmpty, "a new photo starts from the icons")
        await t.settle()
        #expect(t.tile(StyleThumbnails.originalID) != nil && presets.allSatisfy { t.tile($0.id) != nil })
        t.cancel()
    }

    @Test func originalGoesBackToThePhotoAsShotInOneUndoStep() async {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        m.applyPresets([Preset.builtIns[0]])
        let styled = m.focusedEdit
        #expect(styled != EditState())
        let depth = m.history.undoStack.count
        m.resetToOriginal()
        #expect(m.focusedEdit == EditState(), "no adjustments at all")
        #expect(m.history.undoStack.count == depth + 1 && m.history.undoLabel == "Original")
        m.undo()
        #expect(m.focusedEdit == styled, "one undo brings the styled edit back")
    }

    /// User request: a reset-all button. Adjustments ▸ Reset All Adjustments (⇧⌘R, as in
    /// Lightroom) does what the Original row does; off when there's nothing to reset.
    @Test func resetAllAdjustmentsIsTheOriginalRowAsAMenuCommand() async {
        let m = makeModel()
        await m.start()
        let c = MenuCommand.resetAllAdjustments
        #expect(CommandCatalog.title(c, model: m) == "Reset All Adjustments")
        #expect(c.shortcut == MenuCommand.Shortcut(key: "r", modifiers: [.command, .shift]))
        #expect(!CommandCatalog.isEnabled(c, model: m), "no adjustments yet")
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        let edited = m.focusedEdit
        #expect(CommandCatalog.isEnabled(c, model: m))
        CommandCatalog.perform(c, model: m)
        #expect(m.focusedEdit == EditState() && m.history.undoLabel == "Original")
        #expect(!CommandCatalog.isEnabled(c, model: m))
        m.undo()
        #expect(m.focusedEdit == edited)
    }

    @Test func failuresLeaveTheIconAndNoRendererMeansNoTiles() async {
        let t = StyleThumbnails(render: { _, _ in throw MockError.invalid("x") })
        t.refresh(photo: Self.photo(), edit: EditState(), presets: Array(Preset.builtIns.prefix(2)))
        await t.settle()
        #expect(t.images.isEmpty && t.renders == 3)
        let none = StyleThumbnails(render: nil)
        #expect(!none.isAvailable)
        none.refresh(photo: Self.photo(), edit: EditState(), presets: Preset.builtIns)
        await none.settle()
        #expect(none.images.isEmpty)
        #expect(StyleThumbnails.render(with: nil) == nil)
    }
}
