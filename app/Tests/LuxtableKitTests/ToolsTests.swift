import CoreGraphics
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

@Suite("Tools: slider specs")
struct ToolsSliderSpecTests {
    @Test func defaultsMatchIdentityEdit() {
        let e = EditState()
        let pairs: [(SliderSpec, Double)] = [
            (Specs.temperature, e.whiteBalance.temperature), (Specs.tint, e.whiteBalance.tint),
            (Specs.exposure, e.exposure.ev), (Specs.contrast, e.exposure.contrast),
            (Specs.brightness, e.exposure.brightness), (Specs.saturation, e.exposure.saturation),
            (Specs.highlights, e.hdr.highlights), (Specs.shadows, e.hdr.shadows),
            (Specs.whites, e.hdr.whites), (Specs.blacks, e.hdr.blacks),
            (Specs.clarity, e.detail.clarity), (Specs.structure, e.detail.structure),
            (Specs.sharpenAmount, e.detail.sharpenAmount), (Specs.sharpenRadius, e.detail.sharpenRadius),
            (Specs.sharpenThreshold, e.detail.sharpenThreshold), (Specs.noiseLuminance, e.noise.luminance),
            (Specs.noiseColor, e.noise.color), (Specs.noiseDetail, e.noise.detail),
            (Specs.vignetteAmount, e.vignette.amountEV), (Specs.vignetteMidpoint, e.vignette.midpoint),
            (Specs.distortion, e.lens.distortion), (Specs.lightFalloff, e.lens.lightFalloff),
            (Specs.straighten, e.crop.angle), (Specs.wheelLightness, e.colorBalance.master.lightness),
            (Specs.bandHue, e.colorEditor.bands[0].hueShift), (Specs.bandSaturation, e.colorEditor.bands[0].saturation),
            (Specs.bandLightness, e.colorEditor.bands[0].lightness), (Specs.layerOpacity, 1), (Specs.layerExposure, 0),
        ]
        for (spec, identity) in pairs {
            #expect(spec.defaultValue == identity, "\(spec.label)")
            #expect(spec.resetValue == identity)
            #expect(spec.range.contains(identity))
            #expect(spec.isDefault(identity))
            #expect(!spec.help.isEmpty)
        }
    }

    @Test func normalizeClampsAndSnaps() {
        let s = Specs.exposure
        #expect(s.normalize(7) == 5)
        #expect(s.normalize(-9) == -5)
        #expect(abs(s.normalize(0.1234) - 0.12) < 1e-9)
        #expect(s.normalize(.nan) == 0)
        #expect(Specs.temperature.normalize(5503) == 5500)
        let noStep = SliderSpec("x", 0...1, step: 0)
        #expect(noStep.normalize(0.337) == 0.337)
    }

    @Test func fractionRoundTripsAndBipolar() {
        let s = Specs.contrast
        #expect(s.isBipolar)
        #expect(!Specs.sharpenAmount.isBipolar && !Specs.temperature.isBipolar && Specs.tint.isBipolar)
        #expect(SliderSpec.snap(0.3, step: 0.01) == 0.3 && SliderSpec.snap(5503, step: 10) == 5500)
        #expect(SliderSpec.snap(0.37, step: 0) == 0.37 && SliderSpec.snap(0.26, step: 0.25) == 0.25)
        #expect(s.fraction(-100) == 0 && s.fraction(100) == 1 && s.fraction(0) == 0.5)
        #expect(s.fraction(500) == 1)
        #expect(s.value(atFraction: 0.75) == 50)
        #expect(s.value(atFraction: -1) == -100)
        let degenerate = SliderSpec("d", 1...1, default: 1)
        #expect(degenerate.fraction(1) == 0)
    }

    @Test func formatsValues() {
        #expect(Specs.exposure.string(0.3) == "+0.30")
        #expect(Specs.exposure.string(-1) == "-1.00")
        #expect(Specs.exposure.string(0) == "0.00")
        #expect(Specs.contrast.string(25) == "+25")
        #expect(Specs.sharpenAmount.string(140) == "140")
        #expect(Specs.sharpenRadius.string(0.8) == "0.8")
        #expect(Specs.temperature.string(5500).hasSuffix("K"))
        #expect(Specs.vignetteMidpoint.string(0.5) == "50 %")
        #expect(Specs.straighten.string(1.5) == "+1.5°")
    }

    @Test func parsesTypedInput() {
        #expect(Specs.exposure.parse("+0.3") == 0.3)
        #expect(Specs.exposure.parse("-7") == -5)
        #expect(Specs.temperature.parse("5,600 K") == 5600)
        #expect(Specs.vignetteMidpoint.parse("80 %") == 0.8)
        #expect(Specs.straighten.parse("-2°") == -2)
        #expect(Specs.exposure.parse("abc") == nil)
        #expect(Specs.exposure.parse("--") == nil)
    }
}

@Suite("Tools: scrub")
struct ToolsScrubTests {
    @Test func fullRangeOverPointsPerRange() {
        let s = Specs.contrast
        #expect(ScrubMath.value(start: -100, dx: ScrubMath.pointsPerRange, spec: s, fine: false) == 100)
        #expect(ScrubMath.value(start: 0, dx: 15, spec: s, fine: false) == 10)
    }

    @Test func fineIsTenTimesSlowerWithFinerStep() {
        let s = Specs.contrast
        let coarse = ScrubMath.value(start: 0, dx: 30, spec: s, fine: false)
        let fine = ScrubMath.value(start: 0, dx: 30, spec: s, fine: true)
        #expect(coarse == 20)
        #expect(abs(fine - 2) < 1e-9)
        #expect(ScrubMath.value(start: 0, dx: 1, spec: s, fine: true) == 0.1)
        #expect(ScrubMath.value(start: 0, dx: 3, spec: Specs.exposure, fine: false) == 0.1)
    }

    @Test func clampsAtEnds() {
        #expect(ScrubMath.value(start: 4.9, dx: 1000, spec: Specs.exposure, fine: false) == 5)
        #expect(ScrubMath.value(start: -4.9, dx: -1000, spec: Specs.exposure, fine: true) == -5)
        let noStep = SliderSpec("x", 0...1, step: 0)
        #expect(ScrubMath.value(start: 0, dx: 150, spec: noStep, fine: false) == 0.5)
    }
}

@Suite("Tools: curve")
struct ToolsCurveTests {
    @Test func identityRoundTrip() {
        let m = CurveModel(stored: [])
        #expect(m.points == CurveModel.identity)
        #expect(m.isIdentity && m.stored.isEmpty)
        for x in stride(from: 0.0, through: 1, by: 0.1) { #expect(abs(m.evaluate(x) - x) < 1e-9) }
    }

    @Test func addsMissingEndpointsAndSortsClamped() {
        let m = CurveModel(stored: [CurvePoint(0.7, 0.8), CurvePoint(0.3, -0.2)])
        #expect(m.points.map(\.x) == [0, 0.3, 0.7, 1])
        #expect(m.points[1].y == 0)
        #expect(m.stored.count == 4)
    }

    @Test func insertKeepsOrderAndRejectsNearDuplicates() {
        var m = CurveModel(stored: [])
        let a = m.insert(CurvePoint(0.5, 0.6))
        let b = m.insert(CurvePoint(0.25, 0.2))
        #expect(a == 1 && b == 1)
        #expect(m.points.map(\.x) == [0, 0.25, 0.5, 1])
        let near = m.insert(CurvePoint(0.505, 0.1))
        let beyond = m.insert(CurvePoint(1.5, 2)) // clamps onto the endpoint x
        #expect(near == nil && beyond == nil)
    }

    @Test func hitTestPicksNearestWithinRadius() {
        var m = CurveModel(stored: [])
        m.insert(CurvePoint(0.5, 0.5))
        #expect(m.hitTest(CurvePoint(0.52, 0.49), radius: 0.05) == 1)
        #expect(m.hitTest(CurvePoint(0.02, 0.01), radius: 0.05) == 0)
        #expect(m.hitTest(CurvePoint(0.3, 0.8), radius: 0.05) == nil)
    }

    @Test func moveRespectsEndpointsAndNeighbours() {
        var m = CurveModel(stored: [CurvePoint(0, 0), CurvePoint(0.4, 0.4), CurvePoint(0.6, 0.6), CurvePoint(1, 1)])
        let first = m.move(0, to: CurvePoint(0.3, 0.2))
        let last = m.move(3, to: CurvePoint(0.5, 1.4))
        #expect(first == CurvePoint(0, 0.2) && last == CurvePoint(1, 1))
        let moved = m.move(1, to: CurvePoint(0.9, 0.5))
        #expect(abs(moved.x - (0.6 - CurveModel.minGap)) < 1e-12)
        let left = m.move(2, to: CurvePoint(0.1, 0.7))
        #expect(left.x > m.points[1].x)
        let outOfRange = m.move(9, to: CurvePoint(0.5, 0.5))
        #expect(outOfRange == CurvePoint(0.5, 0.5))
    }

    @Test func removeOnlyInnerPoints() {
        var m = CurveModel(stored: [])
        m.insert(CurvePoint(0.5, 0.7))
        let r0 = m.remove(0), r2 = m.remove(2), r7 = m.remove(7)
        #expect(!r0 && !r2 && !r7)
        let r1 = m.remove(1)
        #expect(r1)
        #expect(m.isIdentity)
    }

    @Test func dragOutDetection() {
        #expect(CurveModel.isDraggedOut(CurvePoint(0.5, -0.2)))
        #expect(CurveModel.isDraggedOut(CurvePoint(1.2, 0.5)))
        #expect(CurveModel.isDraggedOut(CurvePoint(-0.1, 0.5)))
        #expect(CurveModel.isDraggedOut(CurvePoint(0.5, 1.1)))
        #expect(!CurveModel.isDraggedOut(CurvePoint(0.5, 1.05)))
    }

    @Test func splinePassesThroughPointsAndIsMonotone() {
        let pts = [CurvePoint(0, 0), CurvePoint(0.25, 0.15), CurvePoint(0.5, 0.6), CurvePoint(0.75, 0.9), CurvePoint(1, 1)]
        let m = CurveModel(stored: pts)
        for p in pts { #expect(abs(m.evaluate(p.x) - p.y) < 1e-9) }
        let s = m.samples()
        #expect(s.count == 256)
        for i in 1..<s.count { #expect(s[i] >= s[i - 1] - 1e-12) }
        #expect(m.samples(count: 1).count == 2)
    }

    @Test func flatSegmentsStayFlatAndNoOvershoot() {
        // A plateau (equal y) must not bulge; a steep step must not overshoot 0...1.
        let m = CurveModel(stored: [CurvePoint(0, 0), CurvePoint(0.3, 0.5), CurvePoint(0.7, 0.5), CurvePoint(0.72, 1), CurvePoint(1, 1)])
        for x in stride(from: 0.3, through: 0.7, by: 0.05) { #expect(abs(m.evaluate(x) - 0.5) < 1e-9) }
        for v in m.samples() { #expect(v >= 0 && v <= 1) }
        // Non-monotone data (peak) keeps extremum tangents at zero.
        let peak = CurveModel(stored: [CurvePoint(0, 0), CurvePoint(0.5, 1), CurvePoint(1, 0)])
        #expect(abs(peak.evaluate(0.5) - 1) < 1e-9)
        #expect(peak.evaluate(0.49) <= 1)
        #expect(peak.evaluate(-1) == 0)
    }
}

@Suite("Tools: wheel")
struct ToolsWheelTests {
    @Test func centerIsNeutral() {
        let v = WheelMath.value(at: CGPoint(x: 50, y: 50), size: 100)
        #expect(v.hue == 0 && v.saturation == 0)
        #expect(WheelMath.value(at: .zero, size: 0).saturation == 0)
    }

    @Test func cardinalDirections() {
        let right = WheelMath.value(at: CGPoint(x: 100, y: 50), size: 100)
        #expect(abs(right.hue) < 1e-9 && abs(right.saturation - 1) < 1e-9)
        let top = WheelMath.value(at: CGPoint(x: 50, y: 0), size: 100)
        #expect(abs(top.hue - 90) < 1e-9)
        let bottom = WheelMath.value(at: CGPoint(x: 50, y: 75), size: 100)
        #expect(abs(bottom.hue - 270) < 1e-9 && abs(bottom.saturation - 0.5) < 1e-9)
        let outside = WheelMath.value(at: CGPoint(x: -100, y: 50), size: 100)
        #expect(abs(outside.hue - 180) < 1e-9 && outside.saturation == 1)
    }

    @Test func pointInvertsValue() {
        for (h, s) in [(0.0, 0.5), (45.0, 1.0), (200.0, 0.3), (359.0, 0.9)] {
            let p = WheelMath.point(hue: h, saturation: s, size: 120)
            let v = WheelMath.value(at: p, size: 120)
            #expect(abs(v.hue - h) < 1e-6 && abs(v.saturation - s) < 1e-9)
        }
        let clamped = WheelMath.point(hue: 0, saturation: 3, size: 100)
        #expect(abs(clamped.x - 100) < 1e-9)
    }

    @Test func hsvToRGBAllSectors() {
        let cases: [(Double, (Double, Double, Double))] = [
            (0, (1, 0, 0)), (60, (1, 1, 0)), (120, (0, 1, 0)), (180, (0, 1, 1)), (240, (0, 0, 1)), (300, (1, 0, 1)), (-60, (1, 0, 1)),
        ]
        for (h, e) in cases {
            let c = WheelMath.rgb(hue: h, saturation: 1, value: 1)
            #expect(abs(c.r - e.0) < 1e-9 && abs(c.g - e.1) < 1e-9 && abs(c.b - e.2) < 1e-9, "hue \(h)")
        }
        let grey = WheelMath.rgb(hue: 123, saturation: 0, value: 0.4)
        #expect(grey.r == 0.4 && grey.g == 0.4 && grey.b == 0.4)
    }

    @Test func previewColorReflectsLightness() {
        let neutral = WheelMath.previewColor(WheelValue())
        #expect(neutral.r == neutral.g && neutral.g == neutral.b)
        let dark = WheelMath.previewColor(WheelValue(hue: 0, saturation: 1, lightness: -1))
        let bright = WheelMath.previewColor(WheelValue(hue: 0, saturation: 1, lightness: 1))
        #expect(dark.r < bright.r && dark.g == 0)
    }
}

@Suite("Tools: levels")
struct ToolsLevelsTests {
    @Test func midpointAndGammaAreInverse() {
        var l = LevelsEdit()
        #expect(abs(LevelsMath.midpoint(l) - 0.5) < 1e-12)
        l.inBlack = 0.1
        l.inWhite = 0.9
        for g in [0.5, 1.0, 2.2] {
            l.gamma = g
            let m = LevelsMath.midpoint(l)
            #expect(abs(LevelsMath.gamma(forMidpoint: m, l) - g) < 1e-9)
            #expect(abs(LevelsMath.transfer(m, l) - 0.5) < 1e-9)
        }
        #expect(LevelsMath.gamma(forMidpoint: 0, l) > 9.9)
        #expect(LevelsMath.gamma(forMidpoint: 1, l) == LevelsMath.gammaRange.lowerBound)
        var flat = LevelsEdit()
        flat.inBlack = 0.5
        flat.inWhite = 0.5
        #expect(LevelsMath.gamma(forMidpoint: 0.5, flat) == 1)
    }

    @Test func inputHandlesKeepOrder() {
        let l = LevelsEdit()
        #expect(LevelsMath.settingInBlack(l, 0.2).inBlack == 0.2)
        #expect(LevelsMath.settingInBlack(l, 1.5).inBlack == 1 - LevelsMath.minGap)
        #expect(LevelsMath.settingInBlack(l, -1).inBlack == 0)
        let b = LevelsMath.settingInBlack(l, 0.4)
        #expect(LevelsMath.settingInWhite(b, 0.1).inWhite == 0.4 + LevelsMath.minGap)
        #expect(LevelsMath.settingInWhite(l, 2).inWhite == 1)
        let m = LevelsMath.settingMidpoint(l, 0.25)
        #expect(abs(m.gamma - 2) < 1e-9)
    }

    @Test func outputHandlesClampAndTransfer() {
        let l = LevelsMath.settingOutWhite(LevelsMath.settingOutBlack(LevelsEdit(), 0.1), 0.9)
        #expect(l.outBlack == 0.1 && l.outWhite == 0.9)
        #expect(LevelsMath.settingOutBlack(l, -3).outBlack == 0)
        #expect(LevelsMath.settingOutWhite(l, 3).outWhite == 1)
        #expect(abs(LevelsMath.transfer(0, l) - 0.1) < 1e-12)
        #expect(abs(LevelsMath.transfer(1, l) - 0.9) < 1e-12)
        #expect(LevelsMath.transfer(0.5, LevelsEdit()) == 0.5)
    }
}

@Suite("Tools: white balance")
struct ToolsWhiteBalanceTests {
    @Test func presetsApplyCustomValues() {
        for p in WhiteBalancePreset.allCases {
            let wb = p.applied(to: WhiteBalanceEdit())
            #expect(!wb.asShot && wb.temperature == p.temperature && wb.tint == p.tint)
            #expect(!p.symbol.isEmpty && p.id == p.rawValue)
        }
        #expect(WhiteBalancePreset.tungsten.temperature < WhiteBalancePreset.daylight.temperature)
        #expect(WhiteBalancePreset.shade.temperature > WhiteBalancePreset.cloudy.temperature)
    }

    @Test func neutralSampleKeepsTemperature() {
        let wb = WhiteBalancePicker.neutralize(WhiteBalanceEdit(), sampledRGB: (0.5, 0.5, 0.5))
        #expect(!wb.asShot)
        #expect(abs(wb.temperature - 5500) < 1e-6 && abs(wb.tint) < 1e-9)
    }

    @Test func warmSampleLowersKelvinCoolRaises() {
        let warm = WhiteBalancePicker.neutralize(WhiteBalanceEdit(), sampledRGB: (0.8, 0.6, 0.4))
        let cool = WhiteBalancePicker.neutralize(WhiteBalanceEdit(), sampledRGB: (0.4, 0.5, 0.8))
        #expect(warm.temperature < 5500)
        #expect(cool.temperature > 5500)
    }

    @Test func greenCastGivesMagentaTint() {
        let green = WhiteBalancePicker.neutralize(WhiteBalanceEdit(), sampledRGB: (0.4, 0.7, 0.4))
        #expect(green.tint > 0)
        let magenta = WhiteBalancePicker.neutralize(WhiteBalanceEdit(), sampledRGB: (0.7, 0.4, 0.7))
        #expect(magenta.tint < 0)
    }

    @Test func buildsOnCustomValuesAndClamps() {
        var custom = WhiteBalanceEdit()
        custom.asShot = false
        custom.temperature = 3000
        custom.tint = 140
        let out = WhiteBalancePicker.neutralize(custom, sampledRGB: (0.2, 0.9, 0.2))
        #expect(out.tint == 150)
        #expect(abs(out.temperature - 3000) < 1e-6)
        let extreme = WhiteBalancePicker.neutralize(WhiteBalanceEdit(), sampledRGB: (0, 0, 1))
        #expect(extreme.temperature <= 50000 && extreme.temperature >= 2000)
    }
}

@Suite("Tools: histogram, layers, archive")
struct ToolsMiscTests {
    @Test func histogramCountsAndClipping() {
        var bytes: [UInt8] = []
        for _ in 0..<998 { bytes += [128, 128, 128, 255] }
        bytes += [255, 255, 255, 255, 0, 0, 0, 255]
        let h = Histogram(frame: RenderedFrame(width: 1000, height: 1, rgba: Data(bytes)))
        #expect(h.pixelCount == 1000)
        #expect(h.red[128] == 998 && h.luma[255] == 1 && h.luma[0] == 1)
        #expect(h.highlightClipped == 1 && h.shadowClipped == 1)
        #expect(!h.clipsHighlights && !h.clipsShadows) // exactly 0.1 % is not above threshold
        let n = h.normalized(.green)
        #expect(n[128] == 1 && n[0] > 0)
        #expect(h.bins(.blue) == h.blue)
        #expect(Histogram().normalized(.luma).allSatisfy { $0 == 0 })
        #expect(Histogram().highlightClipFraction == 0 && Histogram().shadowClipFraction == 0)

        let clipped = Histogram(rgba: Data([255, 0, 0, 255, 0, 0, 255, 255]), pixelCount: 2)
        #expect(clipped.clipsHighlights && clipped.clipsShadows)
    }

    @Test func histogramPolygonAndPolyline() {
        let poly = HistogramPath.polygon([0, 1, 0.5], width: 100, height: 50)
        #expect(poly == [CGPoint(x: 0, y: 50), CGPoint(x: 0, y: 50), CGPoint(x: 50, y: 0), CGPoint(x: 100, y: 25), CGPoint(x: 100, y: 50)])
        #expect(HistogramPath.polygon([0.3], width: 10, height: 10).isEmpty)
        #expect(HistogramPath.polygon([2, -1], width: 10, height: 10)[1].y == 0)
        let line = HistogramPath.polyline([0, 1], width: 10, height: 20)
        #expect(line == [CGPoint(x: 0, y: 20), CGPoint(x: 10, y: 0)])
        #expect(HistogramPath.polyline([], width: 1, height: 1).isEmpty)
    }

    @Test func newLayerIDsAndNames() {
        let first = LayerOps.newLayer(after: [])
        #expect(first.id == 0 && first.name == "Layer 1" && first.enabled && first.opacity == 1)
        let taken = [LayerEdit(id: 4, name: "Layer 2"), LayerEdit(id: 1, name: "Sky")]
        let next = LayerOps.newLayer(after: taken)
        #expect(next.id == 5 && next.name == "Layer 3")
    }

    @Test func archiveDisplayCoversStates() {
        let states: [ArchiveStatus.State] = [.idle, .syncing(fraction: 0.42), .behind(files: 1), .behind(files: 3), .offline, .failed("x")]
        let titles = states.map(ArchiveDisplay.title)
        #expect(titles[1] == "Syncing… 42 %")
        #expect(titles[2] == "Behind by 1 file" && titles[3] == "Behind by 3 files")
        #expect(titles[5] == "Last sync failed", "N1c: never the error text")
        #expect(Set(states.map(ArchiveDisplay.symbol)).count == 5)
    }
}
