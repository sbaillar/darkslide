import CoreGraphics
import DarkslideAI
import Foundation
import Testing
@testable import DarkslideKit

@Suite("Tools: layer operations")
struct LayerOpsTests {
    @Test func duplicateRenameRemove() {
        var layers = [LayerEdit(id: 0, name: "Sky", opacity: 0.5)]
        let copy = LayerOps.duplicate(0, in: &layers)
        #expect(copy == 1 && layers[1].name == "Sky copy" && layers[1].opacity == 0.5)
        LayerOps.duplicate(0, in: &layers)
        #expect(layers[2].name == "Sky copy 2")
        #expect(LayerOps.duplicate(42, in: &layers) == nil)
        LayerOps.rename(1, to: "  Face ", in: &layers)
        LayerOps.rename(1, to: "   ", in: &layers)
        LayerOps.rename(99, to: "x", in: &layers)
        #expect(layers[1].name == "Face")
        LayerOps.remove(0, from: &layers)
        #expect(layers.map(\.id) == [1, 2])
        #expect(LayerOps.newLayer(after: layers, name: "Subject").name == "Subject")
    }

    @Test func addShapeToExistingOrNewLayer() {
        var layers: [LayerEdit] = []
        let id = LayerOps.addShape(.ai(.add, .subject), toLayer: nil, newLayerName: "Subject", in: &layers)
        #expect(id == 0 && layers[0].name == "Subject" && layers[0].mask.shapes == [.ai(.add, .subject)])
        let same = LayerOps.addShape(.fill(.subtract), toLayer: 0, newLayerName: "x", in: &layers)
        #expect(same == 0 && layers[0].mask.shapes.count == 2)
        let other = LayerOps.addShape(.ai(.add, .subject), toLayer: 7, newLayerName: "Subject", in: &layers)
        #expect(other == 1 && layers[1].name == "Subject 2")
    }

    @Test func maskSummary() {
        #expect(LayerOps.maskSummary(MaskEdit()) == "No mask (whole image)")
        let m = MaskEdit(shapes: [.ai(.add, .person), .brush(.add, [BrushStroke(points: [.zero])]),
                                  .linear(.add, start: .zero, end: CGPoint(x: 1, y: 1)),
                                  .radial(.add, center: .zero, radius: CGSize(width: 0.2, height: 0.2), angle: 0, feather: 0.5),
                                  .fill(.add)], invert: true)
        #expect(LayerOps.maskSummary(m) == "Person + Brush (1) + Linear + Radial + Fill, inverted")
    }
}

@MainActor
@Suite("Tools: panel actions and AI masks")
struct PanelActionTests {
    @Test func copyAndApplySingleGroup() async {
        let m = makeModel()
        await m.start()
        m.applyGroup(.exposure)
        #expect(m.toast?.message == "Copy adjustments first")
        m.updateEdit(label: "setup") {
            $0.exposure.ev = 0.5
            $0.hdr.shadows = 30
        }
        m.settings.copyGroups = [.crop, .levels]
        m.copyGroup(.exposure)
        #expect(m.settings.copyGroups == [.crop, .levels], "L6: the Copy options are untouched")
        #expect(m.toast?.message.hasPrefix("Copied Exposure") == true)
        m.perform(.move(.right, extend: false))
        let depth = m.history.undoStack.count
        #expect(m.applyGroup(.exposure) == 1)
        #expect(m.focusedEdit.exposure.ev == 0.5 && m.focusedEdit.hdr.shadows == 0, "only the one group")
        #expect(m.history.undoStack.count == depth + 1)
        m.settings.copyRelativeGroups = [.exposure]
        #expect(m.applyGroup(.exposure) == 1)
        #expect(m.focusedEdit.exposure.ev == 1.0, "relative when remembered")
        m.focus(nil, extend: false)
        let clip = m.clipboard
        m.copyGroup(.hdr)
        #expect(m.clipboard == clip, "no focused image: no copy")
    }

    static func blank() -> CGImage {
        let ctx = CGContext(data: nil, width: 8, height: 8, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        return ctx.makeImage()!
    }

    @Test func aiMaskAddsLayerAndCachesBitmap() async throws {
        let m = makeModel()
        await m.start()
        let fake = AIMasker { target, _ in
            target == .person ? VisionMask(width: 2, height: 1, bytes: [0, 0]) : VisionMask(width: 2, height: 1, bytes: [255, 0])
        }
        let id = try await m.addAIMask(.subject, toLayer: nil, masker: fake)
        #expect(m.focusedEdit.layers.map(\.name) == ["Subject"])
        #expect(m.focusedEdit.layers[0].mask.shapes == [.ai(.add, .subject)])
        #expect(MaskOverlayCache.shared.mask(photo: m.focusedID!, target: .subject)?.bytes == [255, 0])
        #expect(m.history.undoLabel == "AI Subject Mask")
        _ = try await m.addAIMask(.background, toLayer: id, masker: fake)
        #expect(m.focusedEdit.layers.count == 1 && m.focusedEdit.layers[0].mask.shapes.count == 2)
        await #expect(throws: AIMaskError.nothingFound(.person)) { try await m.addAIMask(.person, toLayer: nil, masker: fake) }
        _ = try await m.addAIMask(.sky, toLayer: nil, masker: fake) // [lay9] sky masks through the Core ML model
        #expect(m.focusedEdit.layers.last?.mask.shapes == [.ai(.add, .sky)])
        m.focus(nil, extend: false)
        await #expect(throws: AIMaskError.noImage) { try await m.addAIMask(.subject, toLayer: nil, masker: fake) }
        #expect(AIMaskError.unavailable(.sky).errorDescription?.contains("aren't available") == true)
        #expect(AIMaskError.nothingFound(.person).errorDescription == "No person found in this photo.")
        #expect(AIMaskError.noImage.errorDescription != nil)
        MaskOverlayCache.shared.removeAll()
        #expect(MaskOverlayCache.shared.count == 0)
    }

    @Test func visionMaskerRunsOnDevice() async throws {
        #expect(AIMasker.isAvailable(.sky) && AIMasker.isAvailable(.subject)) // sky: Lay9SkyTests (no download here)
        #expect(AIMasker.help(.sky).contains("sky") && AIMasker.help(.person).contains("Vision"))
        // A blank frame has no subject/person; Vision returns nil or an empty mask, never throws.
        let subject = try await AIMasker.vision.segment(.subject, Self.blank())
        #expect((subject?.coverage ?? 0) < 0.5)
        let bg = try await AIMasker.vision.segment(.background, Self.blank())
        if let subject, let bg { #expect(bg.bytes == subject.bytes.map { 255 - $0 }) }
        _ = try await AIMasker.vision.segment(.person, Self.blank())
    }
}

@MainActor
@Suite("Tools: workspace layout")
struct ToolLayoutTests {
    func defaults() -> (UserDefaults, String) {
        let suite = testSuiteName()
        return (UserDefaults(suiteName: suite)!, suite)
    }

    @Test func reorderAndPersist() {
        let (d, suite) = defaults()
        let l = ToolLayout(defaults: d)
        #expect(l.order == ToolTab.allCases)
        l.move(.metadata, before: .library)
        #expect(l.order.firstIndex(of: .metadata)! < l.order.firstIndex(of: .library)!)
        l.move(.favorites, before: nil)
        #expect(l.order.last == .favorites)
        l.move(.color, before: .color)
        #expect(ToolLayout(defaults: d).order == l.order)
        l.resetOrder()
        #expect(ToolLayout(defaults: d).order == ToolTab.allCases)
        d.removePersistentDomain(forName: suite)
    }

    @Test func sanitizeDropsDuplicatesAndAppendsNewTabs() {
        let (d, suite) = defaults()
        d.set(["color", "color", "bogus", "library"], forKey: ToolLayout.orderKey)
        let l = ToolLayout(defaults: d)
        #expect(Array(l.order.prefix(2)) == [.color, .library])
        #expect(Set(l.order) == Set(ToolTab.allCases) && l.order.count == ToolTab.allCases.count)
        d.removePersistentDomain(forName: suite)
    }

    @Test func favorites() {
        let (d, suite) = defaults()
        let l = ToolLayout(defaults: d)
        #expect(l.panels(for: .favorites) == ToolTab.favorites.panels)
        #expect(l.panels(for: .exposure) == ToolTab.exposure.panels)
        l.toggleFavorite(.curve)
        #expect(l.isFavorite(.curve))
        l.toggleFavorite(.styles)
        #expect(!l.isFavorite(.styles))
        let r = ToolLayout(defaults: d)
        #expect(r.favorites == [.whiteBalance, .exposure, .history, .curve])
        d.removePersistentDomain(forName: suite)
        // The old Favorites key is read once, when there is no workspace yet.
        let (old, oldSuite) = defaults()
        old.set(["curve", "curve", "nope"], forKey: ToolLayout.favoritesKey)
        #expect(ToolLayout(defaults: old).favorites == [.curve])
        old.removePersistentDomain(forName: oldSuite)
    }

    @Test func renamedImportTab() {
        #expect(ToolTab.importArchive.title == "Import & Archive")
        #expect(ToolTab.importArchive.panels == [.importCard, .archive])
        #expect(ToolTab.color.panels.first == .whiteBalance && ToolTab.color.panels.suffix(2) == [.styles, .history]
                && ToolPanelID.styles.group == nil)
        #expect(ToolTab.favorites.symbol == "star")
    }
}
