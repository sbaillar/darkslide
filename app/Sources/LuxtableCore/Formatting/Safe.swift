import Foundation

/// Conversions that never trap on file- or network-derived numbers.
public enum Safe {
    /// `d` rounded to the nearest integer, or nil when it is NaN, infinite or out of `Int` range.
    public static func int(_ d: Double) -> Int? {
        guard d.isFinite else { return nil }
        return Int(exactly: d.rounded())
    }

    /// `d` rounded to the nearest integer, clamped to `Int` range (0 for NaN).
    public static func clampedInt(_ d: Double) -> Int {
        guard !d.isNaN else { return 0 }
        return int(d) ?? (d < 0 ? Int.min : Int.max)
    }
}
