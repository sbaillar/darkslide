import Foundation

/// The settings AI Optimize can change (ds-pipeline `optimize::Field`).
public enum OptimizeField: String, Codable, Sendable, CaseIterable {
    case whiteBalance = "white_balance"
    case exposure, highlights, shadows, levels, contrast, vibrance, saturation, clarity, texture, dehaze
    case aiDenoise = "ai_denoise"
    case noiseReduction = "noise_reduction"
    case straighten
    case lensCorrection = "lens_correction"
    case skyLayer = "sky_layer"
    /// A field a newer core knows and this app doesn't.
    case other

    public init(from decoder: Decoder) throws {
        self = OptimizeField(rawValue: try decoder.singleValueContainer().decode(String.self)) ?? .other
    }

    /// SF Symbol of the row.
    public var symbol: String {
        switch self {
        case .whiteBalance: "thermometer.medium"
        case .exposure: "plusminus.circle"
        case .highlights: "sun.max"
        case .shadows: "moon"
        case .levels: "chart.bar.xaxis"
        case .contrast: "circle.lefthalf.filled"
        case .vibrance, .saturation: "drop"
        case .clarity, .texture: "triangle.lefthalf.filled"
        case .dehaze: "cloud.sun"
        case .aiDenoise, .noiseReduction: "wand.and.rays"
        case .straighten: "level"
        case .lensCorrection: "camera.aperture"
        case .skyLayer: "square.3.layers.3d.top.filled"
        case .other: "slider.horizontal.3"
        }
    }
}

/// One proposed change, as the panel shows it (texts are made in Rust from the
/// measurements; see `ds_pipeline::optimize::explain`).
public struct OptimizeChange: Codable, Equatable, Sendable, Identifiable {
    public var id: Int
    public var field: OptimizeField
    public var label: String
    public var from: String
    public var to: String
    public var confidence: Double
    public var defaultOn: Bool
    public var applied: Bool
    public var explanation: String

    enum CodingKeys: String, CodingKey {
        case id, field, label, from, to, confidence, applied, explanation
        case defaultOn = "default_on"
    }

    public init(from d: Decoder) throws {
        let c = try d.container(keyedBy: CodingKeys.self)
        id = try c.decode(Int.self, forKey: .id)
        field = try c.decode(OptimizeField.self, forKey: .field)
        label = try c.decode(String.self, forKey: .label)
        from = try c.decode(String.self, forKey: .from)
        to = try c.decode(String.self, forKey: .to)
        confidence = try c.decodeIfPresent(Double.self, forKey: .confidence) ?? 0
        defaultOn = try c.decodeIfPresent(Bool.self, forKey: .defaultOn) ?? true
        applied = try c.decodeIfPresent(Bool.self, forKey: .applied) ?? false
        explanation = try c.decode(String.self, forKey: .explanation)
    }
}

/// A field AI Optimize looked at and left alone, and why.
public struct OptimizeKept: Codable, Equatable, Sendable {
    public var field: OptimizeField
    public var label: String
    public var value: String
    public var explanation: String
    /// "user_set", "on_target", "scene_mood".
    public var why: String

    enum CodingKeys: String, CodingKey { case field, label, value, explanation, why }
    enum WhyKeys: String, CodingKey { case why }

    public init(field: OptimizeField, label: String, value: String, explanation: String, why: String) {
        self.field = field
        self.label = label
        self.value = value
        self.explanation = explanation
        self.why = why
    }

    public init(from d: Decoder) throws {
        let c = try d.container(keyedBy: CodingKeys.self)
        field = try c.decode(OptimizeField.self, forKey: .field)
        label = try c.decode(String.self, forKey: .label)
        value = try c.decode(String.self, forKey: .value)
        explanation = try c.decode(String.self, forKey: .explanation)
        why = (try? c.nestedContainer(keyedBy: WhyKeys.self, forKey: .why).decode(String.self, forKey: .why)) ?? ""
    }

    public func encode(to e: Encoder) throws {
        var c = e.container(keyedBy: CodingKeys.self)
        try c.encode(field.rawValue, forKey: .field)
        try c.encode(label, forKey: .label)
        try c.encode(value, forKey: .value)
        try c.encode(explanation, forKey: .explanation)
        var w = c.nestedContainer(keyedBy: WhyKeys.self, forKey: .why)
        try w.encode(why, forKey: .why)
    }

    /// The user set this value (AI Optimize never fights it).
    public var isUserSet: Bool { why == "user_set" }
}

/// AI Optimize's report for one photo. `json` is Rust's report, kept verbatim: it is what
/// `apply` takes and what the sidecar stores; the rest is decoded from it for display.
public struct OptimizeReport: Equatable, Sendable {
    public let json: String
    public let changes: [OptimizeChange]
    public let kept: [OptimizeKept]
    /// The templated summary (no language model writes or rephrases it).
    public let summary: String
    /// Analysis time in Rust (renders included), ms.
    public let elapsedMs: Double

    private struct Body: Decodable {
        var changes: [OptimizeChange]?
        var kept: [OptimizeKept]?
        var summary: String?
        var elapsed_ms: Double?
    }

    public init(json: String) throws {
        let b = try JSONDecoder().decode(Body.self, from: Data(json.utf8))
        self.json = json
        changes = b.changes ?? []
        kept = b.kept ?? []
        summary = b.summary ?? ""
        elapsedMs = b.elapsed_ms ?? 0
    }

    /// The changes checked when the panel opens.
    public var defaultSelection: Set<Int> { Set(changes.filter(\.defaultOn).map(\.id)) }

    /// Changes that were applied (a stored report).
    public var appliedChanges: [OptimizeChange] { changes.filter(\.applied) }

    /// "Exposure +0.40 EV · Highlights +27 · Levels black 8.0%, white 99%" (batch rows).
    public var compactSummary: String {
        changes.isEmpty ? "No changes needed" : changes.map { "\($0.label) \($0.to)" }.joined(separator: " · ")
    }
}
