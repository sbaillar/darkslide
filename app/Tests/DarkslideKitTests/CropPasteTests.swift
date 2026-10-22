import CoreGraphics
import Foundation
import Testing
@testable import DarkslideKit

/// M10: a copied crop lands on another photo by ds-core's `adapt_crop` (through ds-ffi),
/// so an aspect-locked crop keeps its ratio in the target's pixels across portrait and
/// landscape photos.
@MainActor
@Suite struct CropPasteTests {
    /// Long side / short side of `rect` in pixels of a `size` image.
    static func pixelRatio(_ rect: CGRect, _ size: CGSize) -> Double {
        let (w, h) = (rect.width * size.width, rect.height * size.height)
        return max(w, h) / min(w, h)
    }

    static func crop45(portraitOf size: CGSize) -> CropEdit {
        // 4:5 filling the width of the frame.
        var c = CropEdit()
        c.aspect = .r4x5
        let h = size.width * 5 / 4 / size.height
        c.rect = CGRect(x: 0, y: (1 - h) / 2, width: 1, height: h)
        return c
    }

    @Test func aPortrait4x5CropPastesAs4x5PixelsOnALandscapePhoto() async throws {
        let m = makeModel(counts: [40])
        await m.start()
        let portrait = try #require(m.visiblePhotos.first { $0.meta.isPortrait })
        let landscape = try #require(m.visiblePhotos.first { !$0.meta.isPortrait })
        let (ps, ls) = (portrait.pixelSize, landscape.pixelSize)
        m.focus(portrait.id, extend: false)
        let crop = Self.crop45(portraitOf: ps)
        #expect(abs(Self.pixelRatio(crop.rect, ps) - 1.25) < 1e-6)
        m.updateEdit(label: "crop") { $0.crop = crop }
        m.copyAdjustments()
        #expect(m.applyClipboard(to: [landscape.id], options: PasteOptions(groups: [.crop])) == 1)
        let pasted = m.edit(for: landscape.id).crop
        #expect(pasted.aspect == .r4x5)
        let r = Self.pixelRatio(pasted.rect, ls)
        print(String(format: "M10 4:5 portrait crop on a landscape photo: rect %@, %.4f in pixels", "\(pasted.rect)", r))
        #expect(abs(r - 1.25) < 1e-3, "4:5 in the landscape photo's pixels, not the portrait rect verbatim: \(r)")
        #expect(pasted.rect.width * ls.width > pasted.rect.height * ls.height, "it followed its frame: landscape on a landscape photo (ds-core)")
        #expect(CGRect(x: 0, y: 0, width: 1, height: 1).insetBy(dx: -1e-6, dy: -1e-6).contains(pasted.rect))
        // The rect pasted verbatim (the old behaviour) is 2.7:1 on the landscape photo.
        #expect(abs(Self.pixelRatio(crop.rect, ls) - 1.25) > 0.5)

        // ⌥⌘V (copy from previous) follows the same rule.
        let ids = m.visibleIDs
        let i = try #require(ids.firstIndex(of: landscape.id))
        if i > 0 {
            let prev = ids[i - 1]
            let prevSize = try #require(m.photo(prev)).pixelSize
            m.focus(prev, extend: false)
            m.updateEdit(label: "crop") { $0.crop = Self.crop45(portraitOf: prevSize) }
            m.focus(landscape.id, extend: false)
            m.copyFromPrevious()
            let r2 = Self.pixelRatio(m.edit(for: landscape.id).crop.rect, ls)
            #expect(abs(r2 - 1.25) < 1e-3, "copy from previous: \(r2)")
        }
    }

    @Test func adaptFollowsTheCoreRule() {
        let portrait = CGSize(width: 4000, height: 6000), landscape = CGSize(width: 6000, height: 4000)
        // A deliberate portrait 4:5 out of a landscape frame keeps its orientation.
        var c = CropEdit()
        c.aspect = .r4x5
        c.rect = CGRect(x: 0.2, y: 0, width: 3200.0 / 6000.0, height: 1)
        let kept = CropPaste.adapt(c, from: landscape, to: portrait).rect
        #expect(abs(kept.width * portrait.width / (kept.height * portrait.height) - 0.8) < 1e-3, "\(kept)")
        // Unlocked, other orientation: transposed about its centre.
        var free = CropEdit()
        free.aspect = .free
        free.rect = CGRect(x: 0.1, y: 0.2, width: 0.4, height: 0.2)
        let t = CropPaste.adapt(free, from: landscape, to: portrait).rect
        #expect(abs(t.width - 0.2) < 1e-6 && abs(t.height - 0.4) < 1e-6)
        // Unknown sizes (no metadata): pasted as is; other fields never change.
        #expect(CropPaste.adapt(c, from: .zero, to: portrait) == c)
        c.angle = 4
        c.quarterTurns = 1
        let turned = CropPaste.adapt(c, from: landscape, to: portrait)
        #expect(turned.angle == 4 && turned.quarterTurns == 1 && turned.aspect == .r4x5)
        // Without sizes the clipboard keeps its old verbatim paste, but for the rotation:
        // that stays each photo's own (L7, like Copy from Previous).
        let clip = AdjustmentClipboard(source: { var e = EditState(); e.crop = c; return e }())
        var expected = c
        expected.quarterTurns = 0
        #expect(clip.apply(to: EditState(), options: PasteOptions(groups: [.crop])).crop == expected)
    }
}
