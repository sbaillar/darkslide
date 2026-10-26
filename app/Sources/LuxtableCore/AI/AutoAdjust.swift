import LuxtableAI
import Foundation

/// Auto white balance + auto tone from image statistics (pure, unit-tested).
///
/// This is the preview-based estimate (demo mode, and the straighten in live mode). In
/// live mode `AppModel.developAuto` replaces its white balance and tone with
/// `lt_pipeline::auto` on the camera data (GPU statistics).
public enum AutoAdjust {
    /// Neutral reference temperature the UI shows for "no correction".
    public static let neutralKelvin = 5500.0
    /// Gray world over-corrects scenes that are *meant* to be colored (sunsets),
    /// so only part of the measured cast is removed.
    public static let strength = 0.6
    /// Mean sRGB luma an average scene should land on (middle grey ≈ 0.46).
    public static let targetLuma = 0.46

    /// Gray-world white balance: a blue cast (b > r) asks for a warmer
    /// temperature, a green cast (g above the r/b mean) for a magenta tint.
    public static func whiteBalance(meanLinearRGB m: SIMD3<Double>) -> (temperature: Double, tint: Double) {
        let (r, g, b) = (max(m.x, 1e-6), max(m.y, 1e-6), max(m.z, 1e-6))
        let temperature = neutralKelvin * pow(b / r, strength)
        let tint = (g / ((r + b) / 2) - 1) * 100 * strength
        return (min(max(temperature, 2500), 12_000).rounded(), min(max(tint, -50), 50).rounded())
    }

    /// Exposure toward `targetLuma`; shadows/highlights from clipping. Highlights
    /// follow lt-core/Capture One: positive recovers (darkens) them.
    public static func tone(meanLuma: Double, highlightClip: Double, shadowClip: Double) -> (ev: Double, shadows: Double, highlights: Double) {
        func linear(_ v: Double) -> Double { v <= 0.040_45 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4) }
        let mean = max(meanLuma, 0.01)
        var ev = log2(linear(targetLuma) / linear(mean)) * 0.8
        ev = (min(max(ev, -2), 2) * 100).rounded() / 100
        let highlights = highlightClip > 0.005 ? min(60, (highlightClip * 1500).rounded()) : 0
        let shadows = shadowClip > 0.01 ? min(50, (shadowClip * 400).rounded()) : 0
        return (abs(ev) < 0.05 ? 0 : ev, shadows, highlights)
    }

    /// Apply auto WB and auto tone. Auto never straightens: that is an explicit action
    /// (the Straighten tool's Auto button, or "straighten" / "level the horizon" typed).
    public static func apply(statistics s: ImageStatistics, to edit: EditState) -> EditState {
        var e = edit
        let wb = whiteBalance(meanLinearRGB: s.meanLinearRGB)
        e.whiteBalance.asShot = false
        e.whiteBalance.temperature = wb.temperature
        e.whiteBalance.tint = wb.tint
        let t = tone(meanLuma: s.meanLuma, highlightClip: s.highlightClip, shadowClip: s.shadowClip)
        e.exposure.ev = t.ev
        e.hdr.shadows = t.shadows
        e.hdr.highlights = t.highlights
        return e
    }
}

extension EditState {
    /// Apply a natural-language `EditIntent` as deltas, clamped to each tool's range.
    /// `straighten` needs a measured horizon, so it is only honoured when one is passed.
    /// `asShot` (lt-ai `IntentContext.as_shot_wb`): a WB shift on an As Shot photo starts
    /// from the camera's real as-shot Kelvin/tint (C2), else from the neutral reference.
    public func applying(_ intent: EditIntent, horizonDegrees: Double? = nil, asShot: WhiteBalanceValue? = nil) -> EditState {
        var e = self
        if intent.temperatureShift != 0 || intent.tintShift != 0 || intent.warmth != 0 || intent.temperatureTarget != nil {
            if e.whiteBalance.asShot {
                e.whiteBalance.asShot = false
                e.whiteBalance.temperature = asShot?.kelvin ?? AutoAdjust.neutralKelvin
                e.whiteBalance.tint = asShot?.tint ?? 0
            }
            var k = intent.temperatureTarget ?? e.whiteBalance.temperature
            k += intent.temperatureShift
            if intent.warmth != 0 {
                // lt-ai warmth: 100 slider units = 60 mired, warmer = fewer mired.
                let mired = 1_000_000 / max(k, 1) - intent.warmth * 0.6
                k = 1_000_000 / max(mired, 20)
            }
            e.whiteBalance.temperature = min(max(k.rounded(), 2000), 50_000)
            e.whiteBalance.tint = min(max(e.whiteBalance.tint + intent.tintShift, -150), 150)
        }
        e.exposure.ev = min(max(e.exposure.ev + intent.exposureShift, -5), 5)
        e.exposure.contrast = min(max(e.exposure.contrast + intent.contrastShift, -100), 100)
        e.exposure.saturation = min(max(e.exposure.saturation + intent.saturationShift, -100), 100)
        e.hdr.highlights = min(max(e.hdr.highlights + intent.highlightsShift, -100), 100)
        e.hdr.shadows = min(max(e.hdr.shadows + intent.shadowsShift, -100), 100)
        for b in intent.bandShifts where e.colorEditor.bands.indices.contains(b.band) {
            e.colorEditor.bands[b.band].saturation = min(max(e.colorEditor.bands[b.band].saturation + b.saturation, -100), 100)
            e.colorEditor.bands[b.band].lightness = min(max(e.colorEditor.bands[b.band].lightness + b.lightness, -100), 100)
        }
        if let a = intent.cropAspect, let aspect = CropAspect(rawValue: a) { e.crop.aspect = aspect }
        if let d = intent.straightenDegrees { e.crop.angle = min(max(e.crop.angle + d, -45), 45) }
        if intent.straighten, let h = horizonDegrees { e.crop.angle = min(max(-h, -45), 45) }
        return e
    }
}
