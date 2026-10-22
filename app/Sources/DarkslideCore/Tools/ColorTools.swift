import CoreGraphics
import Foundation
import Observation

/// Colour-editor tools that act on the image (Capture One's picker, Lightroom's
/// targeted adjustment tool).
public enum ColorTool: Equatable, Sendable {
    /// Click a colour: adds an Advanced range centred on its hue and selects it.
    case pickRange
    /// Click skin: sets the skin-tone range to its hue.
    case pickSkin
    /// Drag on the image: up/down changes the hue/saturation/lightness of the colour
    /// under the cursor (the two nearest basic bands, weighted by distance).
    case targeted(TargetedChannel)
    /// Click a colour: it becomes the selected layer's colour-range mask refinement.
    case pickMaskRange
    /// Click to heal or clone a spot (source found automatically); drag from the spot
    /// to where the source should come from.
    case spot(SpotKind)
    /// Paint over something to remove it (content-aware fill).
    case remove
}

public enum TargetedChannel: String, CaseIterable, Sendable {
    case hue = "Hue", saturation = "Saturation", lightness = "Lightness"
}

/// Which colour tool is active and which Advanced range the panel edits.
@MainActor @Observable
public final class ColorToolState {
    public var mode: ColorTool? { didSet { if mode != oldValue { onModeChanged?() } } }
    /// The viewer's eyedropper loupe follows the tool.
    @ObservationIgnored var onModeChanged: (() -> Void)?
    /// Index into `ColorEditorEdit.advanced` shown in the panel.
    public var selectedRange: Int?

    /// A targeted-adjustment drag in progress: the photo, where it started, the hue
    /// under the cursor there and the edit before it (the drag is absolute from it).
    struct Drag {
        let photo: PhotoID
        let start: CGPoint
        let hue: Double
        let edit: EditState
    }

    @ObservationIgnored var drag: Drag?
    /// Heal/clone: where the press started (viewer-normalized) and on which photo.
    @ObservationIgnored var spotStart: (photo: PhotoID, point: CGPoint)?
    @ObservationIgnored var spotEnd: CGPoint?
    /// A gradient handle being dragged (Layers).
    @ObservationIgnored var handleDrag: MaskHandleDrag?
    /// Size of new spots (radius, relative to the long edge).
    public var spotRadius = 0.02
    /// The Remove tool's brush and stroke.
    public let removal = RemoveToolState()

    public init() {}

    /// Esc / tool change: leave any colour tool.
    public func cancel() {
        mode = nil
        drag = nil
        spotStart = nil
        removal.stroke = nil
    }
}

/// Pure colour maths for the tools (tested).
public enum ColorPick {
    /// HSV hue in degrees (0..<360) and saturation 0...1 of an RGB triple (0...1).
    public static func hueSaturation(_ rgb: SIMD3<Double>) -> (hue: Double, saturation: Double) {
        let mx = max(rgb.x, rgb.y, rgb.z), mn = min(rgb.x, rgb.y, rgb.z)
        let d = mx - mn
        guard mx > 0, d > 1e-6 else { return (0, 0) }
        var h: Double
        if mx == rgb.x {
            h = (rgb.y - rgb.z) / d
        } else if mx == rgb.y {
            h = 2 + (rgb.z - rgb.x) / d
        } else {
            h = 4 + (rgb.x - rgb.y) / d
        }
        h *= 60
        if h < 0 { h += 360 }
        return (h.truncatingRemainder(dividingBy: 360), d / mx)
    }

    /// An Advanced range for a picked colour: narrower for saturated colours, wider
    /// for muted ones (their hue is less certain).
    public static func range(for rgb: SIMD3<Double>) -> ColorRangeEdit {
        let (h, s) = hueSaturation(rgb)
        return ColorRangeEdit(centerHue: h.rounded(), width: s > 0.4 ? 30 : 50, smoothness: 0.5)
    }

    /// The skin-tone range for a picked skin colour.
    public static func skin(for rgb: SIMD3<Double>, keeping old: SkinToneEdit?) -> SkinToneEdit {
        var s = old ?? SkinToneEdit()
        s.range.centerHue = hueSaturation(rgb).hue.rounded()
        s.range.width = max(s.range.width, 40)
        return s
    }

    /// Circular distance between two hues (degrees, 0...180).
    public static func hueDistance(_ a: Double, _ b: Double) -> Double {
        let d = abs((a - b).truncatingRemainder(dividingBy: 360))
        return min(d, 360 - d)
    }
}

/// Lightroom's targeted adjustment: a drag changes the basic bands around the hue
/// under the cursor, split between the two nearest bands by closeness.
public enum TargetedAdjust {
    /// `(band index, weight)` pairs for `hue` (weights sum to 1).
    public static func weights(hue: Double) -> [(Int, Double)] {
        let hues = ColorEditorEdit.bandHues
        let h = ((hue.truncatingRemainder(dividingBy: 360)) + 360).truncatingRemainder(dividingBy: 360)
        for i in hues.indices {
            let a = hues[i], b = i + 1 < hues.count ? hues[i + 1] : 360
            if h >= a && h < b {
                let t = (h - a) / (b - a)
                return [(i, 1 - t), ((i + 1) % hues.count, t)].filter { $0.1 > 1e-6 }
            }
        }
        return [(0, 1)]
    }

    /// `start` with `amount` (-100...100 of drag) applied to `channel` around `hue`.
    public static func apply(hue: Double, amount: Double, channel: TargetedChannel, to start: EditState) -> EditState {
        var e = start
        for (i, w) in weights(hue: hue) {
            switch channel {
            case .hue:
                e.colorEditor.bands[i].hueShift = (start.colorEditor.bands[i].hueShift + amount * 0.3 * w).clamped(-30, 30)
            case .saturation:
                e.colorEditor.bands[i].saturation = (start.colorEditor.bands[i].saturation + amount * w).clamped(-100, 100)
            case .lightness:
                e.colorEditor.bands[i].lightness = (start.colorEditor.bands[i].lightness + amount * w).clamped(-100, 100)
            }
        }
        return e
    }

    /// Drag distance (normalized, y down) → amount: a full image height up is +200.
    public static func amount(from start: CGPoint, to current: CGPoint) -> Double {
        ((start.y - current.y) * 200).clamped(-100, 100)
    }
}

/// Advanced-range list edits (tested).
public enum ColorRangeOps {
    public static func removing(_ i: Int, from ranges: [ColorRangeEdit]) -> [ColorRangeEdit] {
        guard ranges.indices.contains(i) else { return ranges }
        var out = ranges
        out.remove(at: i)
        return out
    }

    /// Which range stays selected after removing `i` of `count`.
    public static func selection(afterRemoving i: Int, count: Int) -> Int? {
        let left = count - 1
        guard left > 0 else { return nil }
        return min(i, left - 1)
    }
}

extension SkinToneEdit {
    package func with(_ kp: WritableKeyPath<SkinToneEdit, Double>, _ v: Double) -> SkinToneEdit {
        var s = self
        s[keyPath: kp] = v
        return s
    }
}

extension Array {
    package subscript(safe i: Int) -> Element? { indices.contains(i) ? self[i] : nil }
}

/// Slider specs of the colour-editor tools and the layer panel.
public enum ColorToolSpecs {
    public static let rangeCenter = SliderSpec("Hue", 0...359, step: 1, format: .integer, help: "Centre of the picked colour range.")
    public static let rangeWidth = SliderSpec("Width", 5...120, default: 45, step: 1, format: .integer,
                                              help: "How many hues around the centre are included.")
    public static let rangeSmoothness = SliderSpec("Softness", 0...1, default: 0.5, step: 0.01, format: .percent,
                                                   help: "How gradually the range fades out.")
    public static let hueUniformity = SliderSpec("Hue Unify", 0...100, format: .integer,
                                                 help: "Pull skin hues toward one tone (evens out redness).")
    public static let saturationUniformity = SliderSpec("Sat Unify", 0...100, format: .integer,
                                                        help: "Even out skin saturation (blotchiness).")
    public static let lightnessUniformity = SliderSpec("Light Unify", 0...100, format: .integer,
                                                       help: "Even out skin lightness.")
}
