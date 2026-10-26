import Foundation

/// Apply an AI result without clobbering edits made while the AI was running (L5).
///
/// `base` is the edit the AI started from, `result` what it produced from
/// `base`, and `current` the edit now. Only the individual values the AI
/// changed (`result` differs from `base`) are written onto `current`; every
/// other value keeps the user's newer choice. Numbers compare with a tolerance, so a
/// value that only went through JSON and Rust's `f32` storage and back doesn't count as
/// changed; whole groups (crop rect, layers, curve…) compare structurally, in the JSON
/// Rust reads, with the same tolerance on every number in them.
public enum AIEditMerge {
    public static func apply(result: EditState, base: EditState, current: EditState) -> EditState {
        var m = Merge(result: result, base: base, out: current)
        m.take(\.whiteBalance.asShot)
        m.num(\.whiteBalance.temperature)
        m.num(\.whiteBalance.tint)
        m.num(\.exposure.ev)
        m.num(\.exposure.contrast)
        m.num(\.exposure.brightness)
        m.num(\.exposure.saturation)
        m.num(\.presence.vibrance)
        m.num(\.presence.texture)
        m.num(\.presence.dehaze)
        m.num(\.hdr.highlights)
        m.num(\.hdr.shadows)
        m.num(\.hdr.whites)
        m.num(\.hdr.blacks)
        m.num(\.levels.inBlack)
        m.num(\.levels.inWhite)
        m.num(\.levels.gamma)
        m.num(\.levels.outBlack)
        m.num(\.levels.outWhite)
        m.take(\.curve)
        m.take(\.colorBalance)
        for i in result.colorEditor.bands.indices where i < base.colorEditor.bands.count && i < m.out.colorEditor.bands.count {
            m.num(\.colorEditor.bands[i].hueShift)
            m.num(\.colorEditor.bands[i].saturation)
            m.num(\.colorEditor.bands[i].lightness)
        }
        m.num(\.detail.clarity)
        m.num(\.detail.structure)
        m.num(\.detail.sharpenAmount)
        m.num(\.detail.sharpenRadius)
        m.num(\.detail.sharpenThreshold)
        m.num(\.noise.luminance)
        m.num(\.noise.color)
        m.num(\.noise.detail)
        m.take(\.noise.aiDenoise)
        m.num(\.noise.aiStrength)
        m.take(\.vignette)
        m.take(\.lens.enabled)
        m.num(\.lens.distortion)
        m.take(\.lens.chromaticAberration)
        m.num(\.lens.lightFalloff)
        m.num(\.lens.keystoneVertical)
        m.num(\.lens.keystoneHorizontal)
        m.take(\.lens.profile)
        m.take(\.crop.aspect)
        m.num(\.crop.angle)
        m.take(\.crop.rect)
        m.take(\.crop.quarterTurns)
        m.take(\.layers)
        m.take(\.effects)
        // A field the AI set is the AI's now: a look's Intensity leaves it alone.
        let out = m.out
        m.out.look = current.look?.dropping { key in
            EditCodec.scalars.first { $0.0 == key }.map { out[keyPath: $0.1] != current[keyPath: $0.1] } ?? false
        }
        return m.out
    }

    private struct Merge {
        let result: EditState
        let base: EditState
        var out: EditState

        mutating func take<T: Equatable>(_ kp: WritableKeyPath<EditState, T>) {
            let r = result[keyPath: kp]
            guard r != base[keyPath: kp] else { return }
            // Round-off only: the base with the AI's value reads within the tolerance of the
            // base, but not exactly (exactly equal: the JSON doesn't carry it, so trust `!=`).
            var probe = base
            probe[keyPath: kp] = r
            let (a, b) = (AdjustmentsCodec.overlay(probe, onto: [:]), AdjustmentsCodec.overlay(base, onto: [:]))
            if AIEditMerge.near(a, b, tolerance: 1e-6) && !AIEditMerge.near(a, b, tolerance: 0) { return }
            out[keyPath: kp] = r
        }

        mutating func num(_ kp: WritableKeyPath<EditState, Double>) {
            let (r, b) = (result[keyPath: kp], base[keyPath: kp])
            if !AIEditMerge.near(r, b, tolerance: 1e-6) { out[keyPath: kp] = r }
        }
    }

    /// JSON values equal, numbers within `tolerance` (relative, at least absolute).
    static func near(_ a: Any, _ b: Any, tolerance: Double) -> Bool {
        switch (a, b) {
        case let (x as [String: Any], y as [String: Any]):
            x.count == y.count && x.allSatisfy { k, v in y[k].map { near(v, $0, tolerance: tolerance) } ?? false }
        case let (x as [Any], y as [Any]):
            x.count == y.count && zip(x, y).allSatisfy { near($0, $1, tolerance: tolerance) }
        case let (x as String, y as String):
            x == y
        case let (x as NSNumber, y as NSNumber):
            abs(x.doubleValue - y.doubleValue) <= tolerance * max(1, abs(x.doubleValue), abs(y.doubleValue))
        default:
            String(describing: a) == String(describing: b)
        }
    }
}
