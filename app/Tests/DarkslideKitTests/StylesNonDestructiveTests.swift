import CoreGraphics
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Final-wave review: every built-in style applied to a corrected photo leaves the groups
/// it doesn't contain untouched, keeps the photo's EV where it adds to it, and renders.
@MainActor
@Suite(.serialized) struct StylesNonDestructiveTests {
    @Test func theStarterPackIsNonDestructiveAndRenders() async throws {
        guard !sampleARWs().isEmpty else { return }
        let live = try #require(try await Vw8Live.make(limit: 1))
        defer { live.cleanup() }
        let m = live.app
        let id = try #require(m.focusedID)
        _ = try #require(await live.settled())
        // The user's own correction first: +0.7 EV, WB 6000 K, clarity 12, a crop.
        var base = EditState()
        base.exposure.ev = 0.7
        base.whiteBalance.asShot = false
        base.whiteBalance.temperature = 6000
        base.detail.clarity = 12
        base.crop.rect = CGRect(x: 0.05, y: 0.05, width: 0.9, height: 0.9)
        for p in [Preset(name: "none", groups: [.exposure], edit: EditState())] + Preset.builtIns {
            let target = p.name == "none" ? base : p.apply(to: base)
            // Non-destructive: groups the preset doesn't contain are untouched.
            for g in AdjustGroup.allCases where !p.groups.contains(g) {
                #expect(!g.differs(base, target), "\(p.name) changed \(g)")
            }
            if p.groups.contains(.exposure) && !p.absoluteFields.contains("exposure.ev") {
                #expect(target.exposure.ev == base.exposure.ev + p.edit.exposure.ev, "\(p.name) kept the photo's EV")
            }
            let old = m.viewer.frame
            m.updateEdit(label: p.name) { $0 = target }
            m.endEditGesture()
            let f = await live.settled(after: old) ?? m.viewer.frame
            let frame = try #require(f, "\(p.name): no frame")
            #expect(frame.width > 0 && frame.height > 0)
        }
        // Stacking: B&W then Vivid, and a style applied twice.
        let bwv = Preset.apply([Preset.builtIns.first { $0.name == "B&W Neutral" }!, Preset.builtIns.first { $0.name == "Vivid" }!], to: base)
        let punchy = Preset.builtIns.first { $0.name == "Punchy" }!
        let twice = Preset.apply([punchy, punchy], to: base)
        print("STYLES B&W then Vivid: saturation \(bwv.exposure.saturation); Punchy twice: contrast \(twice.exposure.contrast) clarity \(twice.detail.clarity) shadows \(twice.hdr.shadows)")
        _ = id
        // Stacking is additive (review L5, as designed): B&W then Vivid ends muted, not mono.
        #expect(bwv.exposure.saturation > -100)
        #expect(twice.exposure.contrast == 2 * punchy.edit.exposure.contrast + base.exposure.contrast)
    }
}
