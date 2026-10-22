import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation

/// Swift ↔ ds-ai glue (pure conversions, unit-tested). Culling scores, burst
/// grouping and the offline edit parser all live in Rust (`ds-ai`); Swift only
/// supplies pixels, Apple Vision signals and metadata.
public enum DSAI {
    /// Long edge ds-ai's thresholds are tuned for (the A9's embedded preview).
    public static let analysisSize = 1616

    // MARK: Pixels

    /// Tightly packed 8-bit sRGB RGBA of `image` (drawn, so any source format works).
    public static func rgba(_ image: CGImage) -> (width: Int, height: Int, bytes: Data)? {
        let (w, h) = (image.width, image.height)
        guard w > 0, h > 0 else { return nil }
        var data = Data(count: w * h * 4)
        let ok = data.withUnsafeMutableBytes { buf -> Bool in
            guard let ctx = CGContext(data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                                      space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                      bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return false }
            ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
            return true
        }
        return ok ? (w, h, data) : nil
    }

    // MARK: Metadata

    /// Orders frames inside one (whole-second) timestamp: the camera's shot
    /// number, else the file-name sequence (ds-ai `GroupItem.seq`).
    public static func seq(_ meta: PhotoMeta) -> UInt64? {
        (meta.shotNumber ?? meta.fileSequence).flatMap { $0 >= 0 ? UInt64($0) : nil }
    }

    public static func captureMs(_ date: Date) -> Int64 { Int64((date.timeIntervalSince1970 * 1000).rounded()) }

    // MARK: Vision → ds-ai

    /// Vision rects have a bottom-left origin; ds-ai uses top-left.
    static func topLeft(_ r: CGRect) -> NormRect {
        NormRect(x: Float(r.minX), y: Float(1 - r.maxY), w: Float(r.width), h: Float(r.height))
    }

    /// Vision analysis + the camera's AF point as ds-ai `VisionSignals`.
    public static func signals(_ a: DarkslideAI.ImageAnalysis?, afPoint: CGPoint?) -> VisionSignalsRecord {
        let faces = (a?.faces ?? []).map { f in
            FaceSignalRecord(
                eyesOpen: EyeMetrics.openProbability(left: f.leftEyeAspect, right: f.rightEyeAspect).map(Float.init),
                captureQuality: f.captureQuality.map(Float.init),
                area: Float(f.boundingBox.width * f.boundingBox.height),
                confidence: nil,
                bounds: topLeft(f.boundingBox))
        }
        return VisionSignalsRecord(
            faces: faces,
            aesthetics: a?.aesthetics.map { Float($0.overall) },
            isUtility: a?.aesthetics?.isUtility,
            afPoint: afPoint.map { NormPoint(x: Float($0.x), y: Float($0.y)) },
            saliency: (a?.salientRegions ?? []).map(topLeft))
    }

    // MARK: ds-ai results → app

    /// Pick/reject policy on top of ds-ai's per-group suggestion. Conservative:
    /// wrongly rejecting a good frame is worse than keeping a bad one.
    public enum Policy {
        /// A lone frame is picked only when it scores this well on its own.
        public static let lonePick: Float = 0.7
        /// The best of a burst is picked unless it is itself poor.
        public static let groupPick: Float = 0.45
        /// Anything this low is rejected, grouped or not.
        public static let floor: Float = 0.3
    }

    public static func suggestion(for r: CullResultRecord, groupSize: Int) -> CullSuggestion {
        let hard = r.reasons.contains(where: \.hard)
        let pick: PickState
        if r.suggestion == .best, groupSize > 1 {
            // M8: the best frame of a burst is never rejected (at worst it stays undecided);
            // a lone frame is judged on its own below.
            pick = !hard && r.score >= Policy.groupPick ? .picked : .none
        } else if r.suggestion == .reject || hard || r.score < Policy.floor {
            pick = .rejected
        } else if r.suggestion == .best, r.score >= Policy.lonePick {
            pick = .picked
        } else {
            pick = .none
        }
        var reasons = r.reasons.map { capitalized($0.text) }
        if groupSize > 1, r.suggestion == .best { reasons.insert("Best of \(groupSize) similar frames", at: 0) }
        return CullSuggestion(photoID: PhotoID(r.id), score: Double(r.score), pick: pick, reasons: reasons)
    }

    static func capitalized(_ s: String) -> String { s.prefix(1).uppercased() + s.dropFirst() }

    /// Map a ds-ai cull report back onto `photos` (missing ones stay undecided).
    public static func suggestions(_ report: CullReport, photos: [Photo]) -> [CullSuggestion] {
        var sizes: [UInt32: Int] = [:]
        for (i, g) in report.groups.enumerated() { sizes[UInt32(i)] = g.memberIds.count }
        let byID = Dictionary(report.results.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        return photos.map { p in
            guard let r = byID[p.id.raw] else {
                return CullSuggestion(photoID: p.id, score: 0.5, pick: .none, reasons: ["Could not analyze"])
            }
            return suggestion(for: r, groupSize: sizes[r.group] ?? 1)
        }
    }

    // MARK: Edit intents

    static let bands = ["red", "orange", "yellow", "green", "cyan", "blue", "purple", "magenta"]

    /// ds-ai `EditIntent` → the app's `EditIntent`. What needs pixels (auto,
    /// auto white balance) is reported in `unrecognized`.
    public static func intent(from r: EditIntentRecord) -> EditIntent {
        var i = EditIntent(temperatureShift: Double(r.temperatureShiftK), tintShift: Double(r.tint),
                           exposureShift: Double(r.exposureEv), contrastShift: Double(r.contrast),
                           highlightsShift: Double(r.highlights), shadowsShift: Double(r.shadows),
                           saturationShift: Double(r.saturation), straighten: r.straightenAuto)
        i.warmth = Double(r.warmth)
        i.temperatureTarget = r.temperatureK.map(Double.init)
        i.straightenDegrees = r.straightenDegrees.map(Double.init)
        if let w = r.cropWidth, let h = r.cropHeight, w > 0, h > 0 { i.cropAspect = "\(w):\(h)" }
        if r.blackAndWhite == true { i.saturationShift = -100 }
        i.bandShifts = r.bandEdits.compactMap { b in
            bands.firstIndex(of: b.band).map { BandShift(band: $0, saturation: Double(b.saturation), lightness: Double(b.lightness)) }
        }
        i.unrecognized = r.unrecognized
        if r.auto { i.unrecognized.append("auto (use the Auto button: it needs the image)") }
        if r.autoWhiteBalance { i.unrecognized.append("auto white balance (use Auto: it needs the image)") }
        return i.sanitized()
    }
}

/// The offline natural-language fallback: ds-ai's `RuleBasedParser` via FFI.
public struct RulesEditService: NaturalLanguageEditService {
    public init() {}

    public func intent(for text: String) async throws -> EditIntent {
        try DSAI.intent(from: parseEditIntent(text: text))
    }
}

/// Burst stacks from ds-ai's grouping on metadata (capture time, shot/file
/// sequence, camera). Used in live mode; ds-ai resolves A9 whole-second stamps.
public struct BurstGrouping: StackGrouping {
    public init() {}

    public func groups(_ photos: [Photo]) -> [[PhotoID]] {
        let items = photos.map {
            BurstItem(id: $0.id.raw, captureMs: DSAI.captureMs($0.meta.captureDate), seq: DSAI.seq($0.meta),
                      camera: $0.meta.camera.isEmpty ? nil : $0.meta.camera, hash: nil)
        }
        guard let groups = try? groupBurstItems(items: items) else { return CaptureTimeGrouping().groups(photos) }
        return groups.map { $0.memberIds.map(PhotoID.init) }
    }
}

/// Which picks/rejects AI Cull applies (M8), and what the toast says about it.
public enum AICullApplication {
    public struct Plan: Equatable, Sendable {
        public var changes: [PhotoID: PickState]
        public var message: String
    }

    /// Only undecided photos change: no flag, no rating, no colour tag.
    public static func plan(_ photos: [Photo], suggestions: [PhotoID: CullSuggestion]) -> Plan {
        var changes: [PhotoID: PickState] = [:]
        var kept = 0
        for p in photos {
            guard let s = suggestions[p.id], s.pick != .none else { continue }
            if p.pick != .none || p.rating > 0 || p.colorTag != .none {
                kept += 1
                continue
            }
            changes[p.id] = s.pick
        }
        let picks = changes.values.filter { $0 == .picked }.count
        let rejects = changes.values.filter { $0 == .rejected }.count
        var message = changes.isEmpty ? "AI Cull: no changes"
            : "AI Cull: \(picks) \(picks == 1 ? "pick" : "picks"), \(rejects) \(rejects == 1 ? "reject" : "rejects")"
        if kept > 0 { message += "; \(kept) already rated, tagged or flagged left alone" }
        if !changes.isEmpty { message += ". ⌘Z to undo" }
        return Plan(changes: changes, message: message)
    }
}
