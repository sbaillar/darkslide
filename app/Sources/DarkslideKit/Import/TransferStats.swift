import Foundation

/// Throughput (moving average over `window`) and ETA for a running copy.
public struct TransferStats: Equatable, Sendable {
    public struct Sample: Equatable, Sendable {
        public var time: Date
        public var bytes: Int64
    }

    public let window: TimeInterval
    public private(set) var samples: [Sample] = []

    public init(window: TimeInterval = 3) { self.window = window }

    /// Record the cumulative byte count at `time`.
    public mutating func record(bytes: Int64, at time: Date) {
        if let last = samples.last, bytes < last.bytes { samples.removeAll() } // restarted
        samples.append(Sample(time: time, bytes: bytes))
        // Keep one sample at or beyond the window edge as the baseline.
        let cutoff = time.addingTimeInterval(-window)
        while samples.count > 2, samples[1].time <= cutoff { samples.removeFirst() }
    }

    /// Bytes per second over the window, or nil until there are two samples spanning time.
    public var bytesPerSecond: Double? {
        guard let first = samples.first, let last = samples.last else { return nil }
        let dt = last.time.timeIntervalSince(first.time)
        guard dt > 0 else { return nil }
        return Double(last.bytes - first.bytes) / dt
    }

    /// Seconds remaining for `totalBytes`, or nil when the rate is unknown or zero.
    public func eta(totalBytes: Int64) -> TimeInterval? {
        guard let rate = bytesPerSecond, rate > 0, let done = samples.last?.bytes else { return nil }
        return Double(max(0, totalBytes - done)) / rate
    }
}
