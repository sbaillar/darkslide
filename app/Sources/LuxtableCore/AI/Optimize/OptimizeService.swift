import CoreGraphics
import LuxtableAI
import LuxtableFFI
import Foundation

/// Scene types AI Optimize adapts its targets to (lt-pipeline `SceneTag`).
public enum OptimizeSceneTag: String, Sendable, CaseIterable {
    case landscape, portrait, night, snow, beach, sunset, backlit
}

/// What the scene analysis found, in the E frame (the upright, uncropped preview).
public struct OptimizeSignals: Equatable, Sendable {
    public var faces = 0
    /// Total face area, fraction of the frame.
    public var faceArea = 0.0
    /// Degrees, positive = the horizon rises to the right.
    public var horizonDegrees: Double?
    /// The most salient region, normalized, y down.
    public var subject: CGRect?
    public var skyFraction = 0.0
    public var skyMask: VisionMask?
    public var tags: Set<OptimizeSceneTag> = []
    public var aiDenoiseAvailable = false

    public init() {}

    /// Signals from one Vision pass (and the sky model's mask, when it is installed).
    public static func from(_ a: ImageAnalysis?, sky: VisionMask?, aiDenoiseAvailable: Bool) -> OptimizeSignals {
        var s = OptimizeSignals()
        s.aiDenoiseAvailable = aiDenoiseAvailable
        if let sky, sky.coverage > 0.001 {
            s.skyMask = sky
            s.skyFraction = sky.coverage
        }
        guard let a else { return s }
        s.faces = a.faces.count
        s.faceArea = min(1, a.faces.reduce(0) { $0 + Double($1.boundingBox.width * $1.boundingBox.height) })
        s.horizonDegrees = a.horizonDegrees
        // Vision's origin is bottom-left; the pipeline's E frame is y-down.
        s.subject = a.salientRegions.max { $0.width * $0.height < $1.width * $1.height }
            .map { CGRect(x: $0.minX, y: 1 - $0.maxY, width: $0.width, height: $0.height) }
        s.tags = tags(labels: a.labels.map(\.identifier), faces: s.faces, faceArea: s.faceArea)
        return s
    }

    /// Scene tags from Vision's classification labels (and faces: a face filling 3% of
    /// the frame makes a portrait).
    public static func tags(labels: [String], faces: Int, faceArea: Double) -> Set<OptimizeSceneTag> {
        let words: [OptimizeSceneTag: [String]] = [
            .landscape: ["landscape", "mountain", "hill", "valley", "canyon", "cliff", "field", "forest", "lake", "waterfall",
                         "volcano", "desert", "coast", "shore", "outdoor"],
            .portrait: ["portrait", "people", "person", "selfie", "adult", "child"],
            .night: ["night", "night_sky", "starry", "aurora", "fireworks", "moon"],
            .snow: ["snow", "ski", "glacier", "iceberg", "winter"],
            .beach: ["beach", "sand", "seashore"],
            .sunset: ["sunset", "sunrise", "sunset_sunrise", "dusk", "twilight"],
        ]
        var out: Set<OptimizeSceneTag> = []
        for label in labels.map({ $0.lowercased() }) {
            for (tag, ws) in words where ws.contains(where: { label == $0 || label.hasPrefix($0 + "_") }) {
                out.insert(tag)
            }
        }
        if faceArea >= 0.03 { out.insert(.portrait) }
        if faces == 0, faceArea == 0 { out.remove(.portrait) }
        return out
    }

    var ffi: LuxtableFFI.OptimizeSignals {
        LuxtableFFI.OptimizeSignals(
            faces: UInt32(max(0, faces)), faceArea: Float(faceArea), horizonDeg: horizonDegrees.map(Float.init),
            subject: subject.map { NormRect(x: Float($0.minX), y: Float($0.minY), w: Float($0.width), h: Float($0.height)) },
            skyFraction: Float(skyFraction), skyWidth: UInt32(skyMask?.width ?? 0), skyHeight: UInt32(skyMask?.height ?? 0),
            skyMask: Data(skyMask?.bytes ?? []), tags: tags.map(\.rawValue).sorted(),
            aiDenoiseAvailable: aiDenoiseAvailable)
    }
}

public enum OptimizeError: Error, Equatable, LocalizedError {
    case unavailable(String)

    public var errorDescription: String? {
        switch self {
        case .unavailable(let why): why
        }
    }
}

/// Measures a photo and proposes explained changes (lt-pipeline `optimize`), and applies
/// the chosen ones. The values come from deterministic rules, never from a language model.
public protocol PhotoOptimizing: Sendable {
    func analyze(_ photo: Photo, edit: EditState, signals: OptimizeSignals, prior: OptimizeReport?,
                 masks: DevelopRenderer.MaskProvider?) async throws -> OptimizeReport
    /// `edit` with the `selected` changes of `report`, and the report marked with them.
    func apply(_ report: OptimizeReport, selected: Set<Int>, to edit: EditState) throws -> (edit: EditState, report: OptimizeReport)
    /// The ids of the report's applied changes whose values `edit` still has.
    func stillApplied(_ report: OptimizeReport, in edit: EditState) throws -> Set<Int>
}

extension PhotoOptimizing {
    public func stillApplied(_ report: OptimizeReport, in edit: EditState) throws -> Set<Int> {
        try RustPhotoOptimizer().stillApplied(report, in: edit)
    }
}

/// `PhotoOptimizing` over lt-ffi (the shared GPU renderer; CPU fallback).
public struct RustPhotoOptimizer: PhotoOptimizing {
    public init() {}

    public func analyze(_ photo: Photo, edit: EditState, signals: OptimizeSignals, prior: OptimizeReport?,
                        masks: DevelopRenderer.MaskProvider?) async throws -> OptimizeReport {
        guard let url = photo.url else { throw OptimizeError.unavailable("\(photo.filename) has no file") }
        let json = try AdjustmentsCodec.encode(edit)
        var source: StaticMasks?
        let targets = DevelopRenderer.aiTargets(in: edit)
        if !targets.isEmpty, let masks { source = StaticMasks(await masks(photo.id, targets)) }
        let sendableSource = source
        let ffiSignals = signals.ffi
        let out = try await RustDevelopTools.run { token in
            try await optimizeAnalyze(path: url.path, adjustmentsJson: json, signals: ffiSignals,
                                      priorReportJson: prior?.json, masks: sendableSource, token: token)
        }
        return try OptimizeReport(json: out)
    }

    public func apply(_ report: OptimizeReport, selected: Set<Int>,
                      to edit: EditState) throws -> (edit: EditState, report: OptimizeReport) {
        let r = try optimizeApply(adjustmentsJson: try AdjustmentsCodec.encode(edit), reportJson: report.json,
                                  selected: selected.sorted().map { UInt32($0) })
        return (try AdjustmentsCodec.decode(r.adjustmentsJson).0, try OptimizeReport(json: r.reportJson))
    }

    public func stillApplied(_ report: OptimizeReport, in edit: EditState) throws -> Set<Int> {
        Set(try optimizeStillApplied(adjustmentsJson: try AdjustmentsCodec.encode(edit), reportJson: report.json).map(Int.init))
    }
}

/// Stores that keep each photo's last applied optimize report (in its sidecar).
public protocol OptimizeReportStoring: Sendable {
    func optimizeReport(for id: PhotoID) async throws -> OptimizeReport?
    func saveOptimizeReport(_ report: OptimizeReport?, for ids: [PhotoID]) async throws
}
