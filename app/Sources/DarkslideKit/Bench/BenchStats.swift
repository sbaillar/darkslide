import Foundation

/// Median, p90 and n of one operation's timings (docs/BENCHMARKS.md).
public struct BenchSummary: Equatable, Sendable {
    public var n: Int
    /// Samples with no time: the screen never changed before the next stimulus.
    public var missed: Int
    public var median: Double
    public var p90: Double
    public var min: Double
    public var max: Double

    /// Nil when there is no timed sample. The median of an even count is the mean of
    /// the middle two; p90 is the nearest-rank percentile (the value at rank ⌈0.9 n⌉).
    public static func of(_ values: [Double?]) -> BenchSummary? {
        let timed = values.compactMap { $0 }.filter(\.isFinite).sorted()
        guard let lo = timed.first, let hi = timed.last else { return nil }
        let n = timed.count
        let median = n % 2 == 1 ? timed[n / 2] : (timed[n / 2 - 1] + timed[n / 2]) / 2
        let rank = Int((0.9 * Double(n)).rounded(.up))
        return BenchSummary(n: n, missed: values.count - n, median: median, p90: timed[rank - 1], min: lo, max: hi)
    }
}
