import Foundation

/// Small presentation rules for tool panels and counts (kept out of views, tested).
public enum PanelChrome {
    /// Header copy/apply/reset buttons: always visible (keyboard and discoverability, N17),
    /// dimmed until the header is hovered.
    public static func headerButtonOpacity(hovering: Bool) -> Double { hovering ? 1 : 0.4 }

    public static func disclosureLabel(_ title: String, collapsed: Bool) -> String {
        "\(title), \(collapsed ? "collapsed" : "expanded")"
    }

    /// The histogram describes what's on screen: in live mode that is the camera's
    /// embedded JPEG, not the edit.
    public static func histogramTitle(rendersEdits: Bool) -> String {
        rendersEdits ? "Histogram" : "Preview histogram"
    }

    /// "1 photo", "2,000 photos" (the app says "photos" everywhere).
    public static func photoCount(_ n: Int) -> String {
        "\(n.formatted()) photo\(n == 1 ? "" : "s")"
    }
}

/// VoiceOver descriptions for the custom (canvas-drawn) tool controls (N17).
public enum ControlDescriptions {
    public static func wheel(_ title: String, _ v: WheelValue) -> (label: String, value: String) {
        if v == WheelValue() { return ("\(title) color wheel", "neutral") }
        return ("\(title) color wheel",
                "hue \(Safe.clampedInt(v.hue)) degrees, saturation \(Safe.clampedInt((v.saturation * 100))) percent, lightness \(Formatters.signed(v.lightness))")
    }

    public static func curve(_ points: [CurvePoint]) -> (label: String, value: String) {
        guard !points.isEmpty else { return ("Tone curve", "straight line") }
        let list = points.map { "\(Safe.clampedInt(($0.x * 255))) to \(Safe.clampedInt(($0.y * 255)))" }.joined(separator: ", ")
        return ("Tone curve", "\(points.count) point\(points.count == 1 ? "" : "s"): \(list)")
    }

    public static func levels(_ l: LevelsEdit) -> (label: String, value: String) {
        func v(_ x: Double) -> Int { Safe.clampedInt((x * 255)) }
        return ("Levels", "input \(v(l.inBlack)) to \(v(l.inWhite)), gamma \(String(format: "%.2f", l.gamma)), output \(v(l.outBlack)) to \(v(l.outWhite))")
    }

    /// Wheel hue as its own VoiceOver element (M11).
    public static func wheelHue(_ title: String, _ v: WheelValue) -> (label: String, value: String) {
        ("\(title) hue and saturation",
         v.saturation == 0 ? "neutral" : "hue \(Safe.clampedInt(v.hue)) degrees, saturation \(Safe.clampedInt((v.saturation * 100))) percent")
    }
}

/// Keyboard / VoiceOver adjustments for the custom controls (M11): the same edits a drag
/// makes, in small steps. Pure, so they're tested.
public enum ControlAdjust {
    /// Rotate the hue by `steps` × 5° (wrapping); a neutral wheel starts at 10 % saturation
    /// so the step is visible.
    public static func wheelHue(_ v: WheelValue, steps: Int) -> WheelValue {
        var w = v
        w.hue = (v.hue + Double(steps) * 5).truncatingRemainder(dividingBy: 360)
        if w.hue < 0 { w.hue += 360 }
        if w.saturation == 0 { w.saturation = 0.1 }
        return w
    }

    /// Change the saturation by `steps` × 5 %, clamped to 0...1.
    public static func wheelSaturation(_ v: WheelValue, steps: Int) -> WheelValue {
        var w = v
        w.saturation = min(max(v.saturation + Double(steps) * 0.05, 0), 1)
        return w
    }

    /// Brighten (+) or darken (−) the midtones: move (or add) the point at x = 0.5 by
    /// `steps` × 0.02 of the output range.
    public static func curveMidtones(_ points: [CurvePoint], steps: Int) -> [CurvePoint] {
        var pts = points
        let i = pts.firstIndex { abs($0.x - 0.5) < 0.05 } ?? {
            pts.append(CurvePoint(0.5, 0.5))
            pts.sort { $0.x < $1.x }
            return pts.firstIndex { $0.x == 0.5 }!
        }()
        pts[i].y = min(max(pts[i].y + Double(steps) * 0.02, 0), 1)
        return pts
    }

    /// Nudge the midtone gamma by `steps` × 0.05 (clamped to 0.1...10).
    public static func levelsGamma(_ l: LevelsEdit, steps: Int) -> LevelsEdit {
        var e = l
        e.gamma = min(max(l.gamma + Double(steps) * 0.05, 0.1), 10)
        return e
    }

    /// Move the input black (`black`) or white point by `steps` × 1/255, keeping black < white.
    public static func levelsInput(_ l: LevelsEdit, black: Bool, steps: Int) -> LevelsEdit {
        var e = l
        let d = Double(steps) / 255
        if black {
            e.inBlack = min(max(l.inBlack + d, 0), l.inWhite - 1 / 255)
        } else {
            e.inWhite = max(min(l.inWhite + d, 1), l.inBlack + 1 / 255)
        }
        return e
    }
}
