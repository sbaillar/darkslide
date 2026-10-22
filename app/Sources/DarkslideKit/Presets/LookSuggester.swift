import Foundation

/// What the AI Looks pick reads about a photo: the on-device scene analysis (Vision, the
/// same signals AI Optimize uses), its exposure from EXIF, and how dark it renders.
public struct LookSignals: Equatable, Sendable {
    public var tags: Set<OptimizeSceneTag> = []
    public var faces = 0
    /// Total face area, fraction of the frame.
    public var faceArea = 0.0
    /// Sky fraction (0 without the sky model).
    public var skyFraction = 0.0
    public var iso = 0
    /// Seconds.
    public var shutter = 0.0
    /// Median display luma 0...1 of the rendered photo (nil: unknown).
    public var medianLuma: Double?

    public init() {}

    public init(_ s: OptimizeSignals, meta: PhotoMeta?, histogram: Histogram?) {
        tags = s.tags
        faces = s.faces
        faceArea = s.faceArea
        skyFraction = s.skyFraction
        iso = meta?.iso ?? 0
        shutter = meta?.shutter ?? 0
        medianLuma = histogram.flatMap(Self.median)
    }

    /// Median of a luma histogram, 0...1 (nil when empty).
    static func median(_ h: Histogram) -> Double? {
        let total = h.luma.reduce(0) { $0 + Int($1) }
        guard total > 0 else { return nil }
        var seen = 0
        for (i, n) in h.luma.enumerated() {
            seen += Int(n)
            if seen * 2 >= total { return Double(i) / 255 }
        }
        return 1
    }

    /// Shot in low light: night, a high ISO, a slow shutter, or it renders dark.
    var lowLight: Bool {
        tags.contains(.night) || iso >= 3200 || shutter >= 1.0 / 15 || (medianLuma.map { $0 < 0.2 } ?? false)
    }

    var hasPerson: Bool { faces > 0 || tags.contains(.portrait) }
}

/// One AI Looks suggestion and why.
public struct LookSuggestion: Equatable, Sendable, Identifiable {
    public let look: Look
    /// One line: what in the photo made it a fit.
    public let reason: String
    public var id: String { look.id }
}

/// The AI Looks pick: 3–4 looks that suit a photo, by deterministic rules over its on-device
/// signals (no network, no language model), each with its reason.
public enum LookSuggester {
    public static let maxCount = 4

    public static func suggest(_ s: LookSignals) -> [LookSuggestion] {
        var out: [LookSuggestion] = []
        func add(_ id: String, _ reason: String) {
            guard out.count < maxCount, !out.contains(where: { $0.look.id == "look." + id }),
                  let look = LookPack.look(id: "look." + id) else { return }
            out.append(LookSuggestion(look: look, reason: reason))
        }
        if s.hasPerson {
            let share = Safe.clampedInt((s.faceArea * 100))
            add("portrait", s.faces > 0 && share > 0
                ? "A person (face \(share)% of the frame): keep them sharp, soften the background."
                : "A person in the frame: keep them sharp, soften the background.")
        }
        if s.lowLight {
            let why = s.tags.contains(.night) ? "A night scene"
                : s.iso >= 3200 ? "Shot in low light (ISO \(s.iso))"
                : s.shutter >= 1.0 / 15 ? "Shot in low light (a slow shutter)"
                : "A dark photo"
            add("moody", "\(why): deepen the shadows rather than lift the noise.")
            add("noir", "\(why): black and white with grain suits it.")
        }
        if s.tags.contains(.sunset) { add("golden-hour", "A sunset: warm the light it already has.") }
        if s.tags.contains(.snow) || s.tags.contains(.beach) {
            add("bright-airy", s.tags.contains(.snow) ? "Snow: keep it bright and clean." : "A beach: keep it bright and clean.")
        }
        if s.tags.contains(.backlit) { add("dreamy-glow", "Backlit: let the light glow.") }
        if s.tags.contains(.landscape) {
            if s.skyFraction >= 0.25 { add("hdr-detail", "A big sky: hold it and open the land below.") }
            add("golden-hour", "A landscape: warm, late-day light.")
            add("pop", "A landscape: richer colour and crisper detail.")
        }
        if s.hasPerson {
            add("bw-soft", "A portrait: a gentle black and white.")
        }
        // Fill up to three with general-purpose looks.
        for (id, why) in [("cinematic", "A versatile grade for most scenes."), ("pop", "Livelier colour and contrast."),
                          ("faded-matte", "A soft, muted film feel.")] where out.count < 3 {
            add(id, why)
        }
        return out
    }
}

extension AppModel {
    static let lookSignalsLimit = 256

    /// AI Looks for the focused photo, from the on-device scene analysis (empty without one,
    /// or when the focus moved on meanwhile: they would be another photo's). The analysis
    /// runs once per photo; the histogram is read only while it is this photo's.
    public func lookSuggestions() async -> [LookSuggestion] {
        guard let photo = focusedPhoto, !photo.isVideo else { return [] }
        let signals: OptimizeSignals
        if let known = lookSignals[photo.id] {
            signals = known
        } else {
            signals = await optimizeSignals(photo)
            if lookSignals.count >= Self.lookSignalsLimit { lookSignals.removeAll() }
            lookSignals[photo.id] = signals
        }
        guard focusedID == photo.id else { return [] }
        let shown = viewer.frameID == photo.id || viewer.previewID == photo.id
        return LookSuggester.suggest(LookSignals(signals, meta: photo.meta, histogram: shown ? viewer.histogram : nil))
    }
}
