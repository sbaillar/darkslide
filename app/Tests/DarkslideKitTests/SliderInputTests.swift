import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@Suite("Tools: slider keyboard/scroll/drag input")
struct SliderInputTests {
    let ev = Specs.exposure // -5...5, step 0.01
    let contrast = Specs.contrast // -100...100, step 1

    @Test func arrowNudgesByStepShiftAndOption() {
        #expect(SliderInput.nudge(0, .up, spec: contrast) == 1)
        #expect(SliderInput.nudge(0, .down, spec: contrast) == -1)
        #expect(SliderInput.nudge(0, .up, spec: contrast, shift: true) == 10)
        #expect(SliderInput.nudge(0, .up, spec: contrast, option: true) == 0.1)
        #expect(SliderInput.nudge(0.3, .up, spec: ev) == 0.31)
        #expect(SliderInput.nudge(100, .up, spec: contrast, shift: true) == 100, "clamped")
        #expect(SliderInput.nudge(-100, .down, spec: contrast, option: true) == -100)
    }

    @Test func scrollWheelLinesAndPreciseDeltas() {
        #expect(SliderInput.scroll(0, deltaY: 3, precise: false, spec: contrast) == 3)
        #expect(SliderInput.scroll(0, deltaY: -8, precise: true, spec: contrast) == -2, "trackpad: 4 pt per step")
        #expect(SliderInput.scroll(0, deltaY: 1, precise: false, spec: contrast, option: true) == 0.1)
        #expect(SliderInput.scroll(5, deltaY: 0, precise: false, spec: contrast) == 5)
        #expect(SliderInput.scroll(5, deltaY: .nan, precise: false, spec: contrast) == 5)
        #expect(SliderInput.scroll(99, deltaY: 10, precise: false, spec: contrast) == 100)
    }

    @Test func trackDragAbsoluteAndFine() {
        #expect(SliderInput.trackDrag(startValue: 0, startX: 50, x: 100, width: 100, spec: contrast, fine: false) == 100)
        #expect(SliderInput.trackDrag(startValue: 0, startX: 50, x: 25, width: 100, spec: contrast, fine: false) == -50)
        // Fine: 50 pt of a 100 pt track = half the range / 10 = 10 units from the start value.
        #expect(SliderInput.trackDrag(startValue: 20, startX: 50, x: 100, width: 100, spec: contrast, fine: true) == 30)
        #expect(SliderInput.trackDrag(startValue: 0, startX: 0, x: 1, width: 100, spec: ev, fine: true) == 0.01)
        #expect(SliderInput.trackDrag(startValue: 99, startX: 0, x: 1000, width: 0, spec: contrast, fine: true) == 100)
    }

    @Test func panelWidthClampsAndPersists() {
        #expect(PanelWidth.clamp(100) == 260 && PanelWidth.clamp(900) == 480 && PanelWidth.clamp(.nan) == 320)
        #expect(PanelWidth.dragged(start: 320, dx: 40) == 360)
        #expect(PanelWidth.dragged(start: 320, dx: -500) == 260)
        let suite = testSuiteName()
        let d = UserDefaults(suiteName: suite)!
        #expect(PanelWidth.stored(d) == PanelWidth.default)
        d.set(1000.0, forKey: PanelWidth.storageKey)
        #expect(PanelWidth.stored(d) == 480)
        d.removePersistentDomain(forName: suite)
    }

    @MainActor
    @Test func controlFocusCountsHandovers() {
        let f = ControlFocus()
        #expect(!f.isToolControlFocused)
        f.focusChanged(true)
        f.focusChanged(true) // new control gains focus before the old one reports losing it
        f.focusChanged(false)
        #expect(f.isToolControlFocused)
        f.focusChanged(false)
        f.focusChanged(false) // spurious extra loss never goes negative
        #expect(!f.isToolControlFocused && f.focusedCount == 0)
    }
}

@Suite("Tools: theme contrast (WCAG)")
struct PaletteTests {
    @Test func textTokensMeetAAOnPanelAndChrome() {
        for (name, palette) in [("dark", GreyPalette.dark), ("light", GreyPalette.light)] {
            for pair in palette.textPairs {
                let r = Contrast.ratio(grey: pair.background, grey: pair.foreground)
                #expect(r >= 4.5, "\(name) \(pair.name): \(r)")
            }
        }
    }

    @Test func textHierarchyIsOrdered() {
        let d = GreyPalette.dark, l = GreyPalette.light
        #expect(d.textPrimary > d.textSecondary && d.textSecondary > d.textTertiary)
        #expect(l.textPrimary < l.textSecondary && l.textSecondary < l.textTertiary)
    }

    @Test func contrastMath() {
        #expect(abs(Contrast.ratio(grey: 0, grey: 1) - 21) < 1e-9)
        #expect(Contrast.ratio(grey: 0.5, grey: 0.5) == 1)
        #expect(Contrast.ratio(0.2, 0.9) == Contrast.ratio(0.9, 0.2))
        #expect(abs(Contrast.luminance(r: 1, g: 1, b: 1) - 1) < 1e-9)
        #expect(Contrast.luminance(-1) == 0 && Contrast.luminance(0.02) == 0.02 / 12.92)
    }
}

@Suite("Tools: histogram readout")
struct HistogramReadoutTests {
    @Test func clipText() {
        #expect(HistogramReadout.clip(0) == "0 %")
        #expect(HistogramReadout.clip(.nan) == "0 %")
        #expect(HistogramReadout.clip(0.00005) == "<0.01 %")
        #expect(HistogramReadout.clip(0.0012) == "0.12 %")
        #expect(HistogramReadout.clip(0.253) == "25.3 %")
    }

    @Test func rgb() {
        #expect(HistogramReadout.rgb(1, 0.5, -1) == ["255", "128", "0"])
    }
}
