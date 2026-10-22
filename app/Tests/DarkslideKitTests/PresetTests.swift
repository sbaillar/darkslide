import Foundation
import Testing
@testable import DarkslideKit

@Suite("Presets: codec, stacking, store")
struct PresetTests {
    func edited() -> EditState {
        var e = EditState()
        e.whiteBalance.asShot = false
        e.whiteBalance.temperature = 6200
        e.exposure.ev = 0.4
        e.hdr.shadows = 30
        e.levels.gamma = 1.3
        e.curve.points[.rgb] = [CurvePoint(0.25, 0.2), CurvePoint(0.75, 0.8)]
        e.colorBalance.midtones = WheelValue(hue: 120, saturation: 0.2, lightness: -0.1)
        e.colorEditor.bands[5].saturation = -30
        e.detail.clarity = 12
        e.noise.color = 40
        e.vignette.amountEV = -0.7
        e.lens.chromaticAberration = false
        e.crop.aspect = .r4x5
        e.crop.angle = 1.5
        return e
    }

    @Test func codecRoundTripsEverythingButLayers() throws {
        var e = edited()
        e.layers = [LayerEdit(id: 0, name: "x")]
        let data = try JSONEncoder().encode(EditCodec.encode(e))
        let back = EditCodec.decode(try JSONDecoder().decode(EditSnapshot.self, from: data))
        var expected = e
        expected.layers = []
        #expect(back == expected)
        #expect(EditCodec.decode(EditCodec.encode(EditState())) == EditState())
    }

    @Test func codecToleratesUnknownMissingAndBadValues() {
        let s = EditSnapshot(values: ["exposure.ev": 1, "future.key": 3, "hdr.shadows": .nan, "crop.aspect": 99],
                             curves: ["Nope": [[0, 0]], "Luma": [[0.5, 0.6], [1]]])
        let e = EditCodec.decode(s)
        #expect(e.exposure.ev == 1 && e.hdr.shadows == 0 && e.crop.aspect == .original)
        #expect(e.curve.points[.luma] == [CurvePoint(0.5, 0.6)])
    }

    @Test func presetsDropLayersAndNonRelativeGroups() {
        let p = Preset(name: "x", groups: [.exposure, .layers, .levels], relative: [.exposure, .levels, .hdr], edit: edited())
        #expect(p.groups == [.exposure, .levels])
        #expect(p.relative == [.exposure])
    }

    @Test func applyAndStack() {
        var target = EditState()
        target.exposure.ev = 1
        let punchy = Preset.builtIns.first { $0.name == "Punchy" }!
        let once = punchy.apply(to: target)
        #expect(once.exposure.ev == 1, "relative exposure keeps the photo's own EV")
        #expect(once.exposure.contrast == 22 && once.hdr.highlights == -20 && once.detail.clarity == 20)
        let twice = Preset.apply([punchy, punchy], to: target)
        #expect(twice.exposure.contrast == 44, "relative groups stack")
        #expect(twice.detail.clarity == 20, "fields a style sets are set, not added")
        let bw = Preset.builtIns.first { $0.name == "B&W High Contrast" }!
        #expect(bw.apply(to: target).exposure.saturation == -100)
        #expect(bw.apply(to: twice).exposure.ev == 1, "no style resets the photo's exposure (M9)")
        #expect(Preset.apply([], to: target) == target)
    }

    @Test func builtInsAreStableAndDistinct() {
        #expect(Preset.builtIns.count == 20)
        #expect(Set(Preset.builtIns.map(\.id)).count == 20 && Preset.builtIns.allSatisfy(\.isBuiltIn))
        #expect(Preset.builtIns.map(\.id) == Preset.builtIns.map(\.id))
    }

    func tempURL() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("ds-presets-\(UUID().uuidString)/presets.json")
    }

    @MainActor
    @Test func storeSavesLoadsRenamesDeletes() throws {
        let url = tempURL()
        let store = PresetStore(fileURL: url)
        #expect(store.userPresets.isEmpty && store.all.count == 20 && store.lastError == nil)
        #expect(store.save(name: "  ", edit: edited(), groups: [.exposure]) == nil)
        #expect(store.lastError == "Give the preset a name.")
        #expect(store.save(name: "x", edit: edited(), groups: [.layers]) == nil)
        #expect(store.lastError == "Choose at least one adjustment group.")
        let a = try #require(store.save(name: "Warm Film", edit: edited(), groups: [.whiteBalance, .exposure], relative: [.exposure]))
        #expect(a.name == "Warm Film 2", "names are unique across built-ins")
        let b = try #require(store.save(name: "Mine", edit: edited(), groups: [.crop]))
        store.rename(b.id, to: "  Portrait crop ")
        store.rename(b.id, to: " ")
        store.setAutoApply(b.id)
        #expect(store.autoApplyPreset?.id == b.id)

        let reloaded = PresetStore(fileURL: url)
        #expect(reloaded.userPresets.map(\.name) == ["Warm Film 2", "Portrait crop"])
        #expect(reloaded.userPresets[0].relative == [.exposure])
        #expect(reloaded.userPresets[0].edit.whiteBalance.temperature == 6200)
        #expect(reloaded.autoApplyPreset?.name == "Portrait crop")
        reloaded.setAutoApply(nil)
        #expect(reloaded.autoApplyPreset == nil)
        reloaded.delete(a.id)
        #expect(PresetStore(fileURL: url).userPresets.map(\.name) == ["Portrait crop"])
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }

    @MainActor
    @Test func storeReportsCorruptFilesAndWorksWithoutFile() throws {
        let url = tempURL()
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data("not json".utf8).write(to: url)
        let bad = PresetStore(fileURL: url)
        #expect(bad.userPresets.isEmpty)
        #expect(bad.lastError?.hasPrefix("Couldn’t read presets") == true)
        let memory = PresetStore(fileURL: nil)
        #expect(memory.save(name: "m", edit: edited(), groups: [.exposure]) != nil)
        #expect(memory.lastError == nil)
        // Unwritable location: the directory path is a file.
        let blocker = FileManager.default.temporaryDirectory.appendingPathComponent("ds-block-\(UUID().uuidString)")
        try Data().write(to: blocker)
        let unwritable = PresetStore(fileURL: blocker.appendingPathComponent("p.json"))
        _ = unwritable.save(name: "n", edit: edited(), groups: [.exposure])
        #expect(unwritable.lastError?.hasPrefix("Couldn’t save presets") == true)
        #expect(PresetStore.defaultURL?.lastPathComponent == "presets.json")
        try? FileManager.default.removeItem(at: blocker)
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }

    @MainActor
    @Test func appModelAppliesAndPreviewsPresets() async {
        let m = makeModel()
        await m.start()
        let punchy = Preset.builtIns.first { $0.name == "Punchy" }!
        #expect(m.previewEdit(applying: punchy)?.exposure.contrast == 22)
        #expect(m.focusedEdit.isIdentity, "preview doesn't change the edit")
        let depth = m.history.undoStack.count
        m.applyPresets([punchy, punchy])
        #expect(m.focusedEdit.exposure.contrast == 44)
        #expect(m.history.undoStack.count == depth + 1 && m.history.undoLabel == "2 Styles")
        m.applyPresets([punchy])
        #expect(m.history.undoLabel == "Style Punchy")
        m.applyPresets([])
        #expect(m.history.undoLabel == "Style Punchy")
        m.focus(nil, extend: false)
        #expect(m.previewEdit(applying: punchy) == nil)
    }
}
