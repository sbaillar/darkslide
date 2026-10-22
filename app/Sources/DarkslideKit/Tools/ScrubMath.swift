import Foundation

/// Drag-to-scrub on a numeric field: horizontal pixels → value change.
public enum ScrubMath {
    /// Points of horizontal drag to sweep the whole range at normal speed.
    public static let pointsPerRange: Double = 300
    /// Option-drag divides the speed by this.
    public static let fineFactor: Double = 10

    /// New value after dragging `dx` points from `start`. Normalized (clamped + snapped) by `spec`;
    /// fine mode snaps to a tenth of the step so small moves still register.
    public static func value(start: Double, dx: Double, spec: SliderSpec, fine: Bool) -> Double {
        let span = spec.range.upperBound - spec.range.lowerBound
        let delta = dx / pointsPerRange * span / (fine ? fineFactor : 1)
        let raw = start + delta
        let clamped = min(max(raw, spec.range.lowerBound), spec.range.upperBound)
        let step = fine ? spec.step / fineFactor : spec.step
        return min(max(SliderSpec.snap(clamped, step: step), spec.range.lowerBound), spec.range.upperBound)
    }
}
