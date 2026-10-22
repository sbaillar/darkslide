import DarkslideCore
import DarkslideUI
import Foundation

/// A small grayscale copy of the recorded screen region at one instant.
public struct LumaFrame: Sendable, Equatable {
    /// Seconds on the same clock as the stimuli (host time).
    public var t: Double
    public var width: Int
    public var height: Int
    /// Row-major 8-bit luma, `width * height` values.
    public var pixels: [UInt8]

    public init(t: Double, width: Int, height: Int, pixels: [UInt8]) {
        precondition(pixels.count == width * height, "luma size")
        self.t = t
        self.width = width
        self.height = height
        self.pixels = pixels
    }
}

/// On-screen latency from a screen recording, the same way for every app: when did the
/// region first change after a stimulus, and when did it stop changing.
///
/// A pixel counts as changed when its luma moved more than `pixelThreshold` (above
/// scaling and dither noise); a frame counts as changed when at least
/// `minChangedFraction` of the region did (above a blinking cursor or a progress
/// spinner). The full-quality render is the first change followed by `settle` seconds
/// of quiet (apps refine a preview in place, so the end of that burst is when the
/// final pixels landed).
public struct FrameDiff: Sendable, Equatable {
    public var pixelThreshold: Int
    public var minChangedFraction: Double
    public var settle: Double
    /// A frame where at least this fraction changed is a new photo on screen (a refine of
    /// the same photo changes far less), for `measureSequence`.
    public var switchFraction: Double

    public init(pixelThreshold: Int = 16, minChangedFraction: Double = 0.01, settle: Double = 0.5,
                switchFraction: Double = 0.25) {
        self.switchFraction = switchFraction
        self.pixelThreshold = pixelThreshold
        self.minChangedFraction = minChangedFraction
        self.settle = settle
    }

    public struct Timing: Sendable, Equatable {
        public var stimulus: Double
        /// First changed frame after the stimulus (nil: none before the next stimulus).
        public var firstChange: Double?
        /// Last change before `settle` seconds of quiet (nil: never settled in its window).
        public var settled: Double?

        public var firstChangeMs: Double? { firstChange.map { ($0 - stimulus) * 1000 } }
        public var settledMs: Double? { settled.map { ($0 - stimulus) * 1000 } }
    }

    /// Fraction of pixels that changed between two frames of the same size (1 when the
    /// sizes differ: a resized region is a change).
    public func changedFraction(_ a: LumaFrame, _ b: LumaFrame) -> Double {
        guard a.width == b.width, a.height == b.height, !a.pixels.isEmpty else { return 1 }
        var changed = 0
        for i in 0..<a.pixels.count where abs(Int(a.pixels[i]) - Int(b.pixels[i])) > pixelThreshold {
            changed += 1
        }
        return Double(changed) / Double(a.pixels.count)
    }

    /// Held keys, matched in order.
    public struct Sequence: Sendable, Equatable {
        /// One latency (ms) per press, nil when it was missed.
        public var latencies: [Double?]
        /// Frames detected as a new photo.
        public var switches: Int
        /// More new photos than presses: something other than a new photo (a refine of
        /// the same photo, an animation) crossed `switchFraction`, which would credit
        /// later presses with earlier changes. The step's numbers must not be used.
        public var valid: Bool
    }

    /// Held keys: one latency per press, matched in order. The k-th photo change after
    /// the presses began answers the k-th press (and never comes before it), so an app that
    /// keeps up but lags 150 ms shows 150 ms, not the 50 ms left until the next press's
    /// window. Presses with no change of their own are nil: missed.
    ///
    /// A "photo change" is a frame where at least `switchFraction` of the region changed.
    /// That threshold is not yet validated on real recordings: an app that refines a photo
    /// in place (embedded preview, then its own render) can cross it, and then a later
    /// press is credited with an earlier change, which flatters that app. So a step with
    /// more changes than presses is marked invalid.
    public func measureSequence(frames: [LumaFrame], stimuli: [Double], end: Double) -> Sequence {
        let frames = frames.sorted { $0.t < $1.t }
        let first = stimuli.min() ?? .infinity
        var switches: [Double] = []
        for i in frames.indices.dropFirst() where frames[i].t > first && frames[i].t <= end
            && changedFraction(frames[i - 1], frames[i]) >= switchFraction {
            switches.append(frames[i].t)
        }
        var next = 0
        let latencies = stimuli.sorted().map { s -> Double? in
            while next < switches.count, switches[next] <= s { next += 1 }
            guard next < switches.count else { return nil }
            defer { next += 1 }
            return (switches[next] - s) * 1000
        }
        return Sequence(latencies: latencies, switches: switches.count, valid: switches.count <= stimuli.count)
    }

    /// One timing per stimulus. Each stimulus owns the frames up to the next one
    /// (the last one owns frames up to `end`, when recording stopped). Frames need not
    /// be evenly spaced: a recorder that drops unchanged frames is fine, since quiet
    /// time is measured between the changes themselves.
    public func measure(frames: [LumaFrame], stimuli: [Double], end: Double) -> [Timing] {
        let frames = frames.sorted { $0.t < $1.t }
        let stimuli = stimuli.sorted()
        // Times of frames that differ from the frame before them.
        var changes: [Double] = []
        for i in frames.indices.dropFirst() where changedFraction(frames[i - 1], frames[i]) >= minChangedFraction {
            changes.append(frames[i].t)
        }
        return stimuli.enumerated().map { i, s in
            let windowEnd = i + 1 < stimuli.count ? stimuli[i + 1] : end
            let inWindow = changes.filter { $0 > s && $0 < windowEnd }
            // Settled: the end of the first burst of changes followed by `settle` of quiet.
            let settled = inWindow.indices.first { j in
                (j + 1 < inWindow.count ? inWindow[j + 1] : windowEnd) - inWindow[j] >= settle
            }.map { inWindow[$0] }
            return Timing(stimulus: s, firstChange: inWindow.first, settled: settled)
        }
    }
}
