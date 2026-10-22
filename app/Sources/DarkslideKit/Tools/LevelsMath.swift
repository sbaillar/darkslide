import Foundation

/// Levels handles. Transfer: out = outBlack + (outWhite - outBlack) * t^(1/gamma),
/// with t = (x - inBlack) / (inWhite - inBlack). The midtone handle sits at the
/// input where the output is half way: t = 0.5^gamma.
public enum LevelsMath {
    public static let minGap = 0.02
    public static let gammaRange = 0.1...10.0

    /// Absolute input position (0...1) of the gamma handle.
    public static func midpoint(_ l: LevelsEdit) -> Double {
        l.inBlack + (l.inWhite - l.inBlack) * pow(0.5, l.gamma)
    }

    /// Gamma that puts the midtone handle at absolute input `x`.
    public static func gamma(forMidpoint x: Double, _ l: LevelsEdit) -> Double {
        let span = l.inWhite - l.inBlack
        guard span > 0 else { return 1 }
        let t = min(max((x - l.inBlack) / span, 0.001), 0.999)
        return min(max(log(t) / log(0.5), gammaRange.lowerBound), gammaRange.upperBound)
    }

    public static func settingInBlack(_ l: LevelsEdit, _ v: Double) -> LevelsEdit {
        var o = l
        o.inBlack = min(max(v, 0), l.inWhite - minGap)
        return o
    }

    public static func settingInWhite(_ l: LevelsEdit, _ v: Double) -> LevelsEdit {
        var o = l
        o.inWhite = max(min(v, 1), l.inBlack + minGap)
        return o
    }

    public static func settingMidpoint(_ l: LevelsEdit, _ x: Double) -> LevelsEdit {
        var o = l
        o.gamma = gamma(forMidpoint: x, l)
        return o
    }

    /// Output handles may cross (inverting output is allowed in C1); they only clamp to 0...1.
    public static func settingOutBlack(_ l: LevelsEdit, _ v: Double) -> LevelsEdit {
        var o = l
        o.outBlack = min(max(v, 0), 1)
        return o
    }

    public static func settingOutWhite(_ l: LevelsEdit, _ v: Double) -> LevelsEdit {
        var o = l
        o.outWhite = min(max(v, 0), 1)
        return o
    }

    /// The levels transfer function for drawing.
    public static func transfer(_ x: Double, _ l: LevelsEdit) -> Double {
        let span = max(l.inWhite - l.inBlack, 1e-6)
        let t = min(max((x - l.inBlack) / span, 0), 1)
        return l.outBlack + (l.outWhite - l.outBlack) * pow(t, 1 / max(l.gamma, 0.01))
    }
}
