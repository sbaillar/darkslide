import Foundation
#if canImport(FoundationModels)
import FoundationModels
#endif

/// A local color edit on one of the eight Basic color-editor bands.
public struct BandShift: Sendable, Equatable {
    /// 0 red, 1 orange, 2 yellow, 3 green, 4 cyan, 5 blue, 6 purple, 7 magenta.
    public var band: Int
    public var saturation: Double
    public var lightness: Double

    public init(band: Int, saturation: Double, lightness: Double) {
        self.band = band
        self.saturation = saturation
        self.lightness = lightness
    }
}

/// A typed edit extracted from text like "warmer, crop 4:5, lift shadows".
/// Every field is a *delta* in the tools' UI units (ds-core conventions);
/// zero means "not mentioned".
public struct EditIntent: Sendable, Equatable {
    /// Kelvin, -3000...3000 (positive = warmer).
    public var temperatureShift: Double = 0
    /// Warmer (+) / cooler (−) in -100...100 slider units; 100 = 60 mired (ds-ai `warmth`).
    public var warmth: Double = 0
    /// Set the white balance to this temperature (K) before the shifts.
    public var temperatureTarget: Double?
    /// Straighten by this many degrees, counter-clockwise positive.
    public var straightenDegrees: Double?
    public var bandShifts: [BandShift] = []
    /// Clauses the parser didn't understand (shown to the user, never dropped silently).
    public var unrecognized: [String] = []
    /// -50...50 (positive = magenta).
    public var tintShift: Double = 0
    /// Stops, -3...3.
    public var exposureShift: Double = 0
    /// -100...100 each. Highlights: positive recovers (darkens) them, as in
    /// Capture One and ds-core.
    public var contrastShift: Double = 0
    public var highlightsShift: Double = 0
    public var shadowsShift: Double = 0
    public var saturationShift: Double = 0
    /// "1:1", "4:5", "3:2", "16:9"; `nil` = leave the crop alone.
    public var cropAspect: String?
    /// Level the horizon automatically.
    public var straighten = false

    public init(temperatureShift: Double = 0, tintShift: Double = 0, exposureShift: Double = 0, contrastShift: Double = 0,
                highlightsShift: Double = 0, shadowsShift: Double = 0, saturationShift: Double = 0,
                cropAspect: String? = nil, straighten: Bool = false) {
        self.temperatureShift = temperatureShift
        self.tintShift = tintShift
        self.exposureShift = exposureShift
        self.contrastShift = contrastShift
        self.highlightsShift = highlightsShift
        self.shadowsShift = shadowsShift
        self.saturationShift = saturationShift
        self.cropAspect = cropAspect
        self.straighten = straighten
    }

    public static let cropAspects = ["1:1", "4:5", "3:2", "16:9"]

    /// Nothing to apply (ignores `unrecognized`).
    public var isEmpty: Bool {
        var copy = self
        copy.unrecognized = []
        return copy == EditIntent()
    }

    /// Clamp to the documented ranges and normalize the crop aspect (e.g. "5:4" → "4:5", "none" → nil).
    public func sanitized() -> EditIntent {
        func c(_ v: Double, _ lim: Double) -> Double { v.isFinite ? min(max(v, -lim), lim) : 0 }
        var out = EditIntent(temperatureShift: c(temperatureShift, 3000), tintShift: c(tintShift, 50),
                             exposureShift: c(exposureShift, 3), contrastShift: c(contrastShift, 100),
                             highlightsShift: c(highlightsShift, 100), shadowsShift: c(shadowsShift, 100),
                             saturationShift: c(saturationShift, 100), straighten: straighten)
        out.warmth = c(warmth, 100)
        out.temperatureTarget = temperatureTarget.flatMap { $0.isFinite ? min(max($0, 2000), 50_000) : nil }
        out.straightenDegrees = straightenDegrees.flatMap { $0.isFinite ? c($0, 45) : nil }
        out.bandShifts = bandShifts.filter { (0..<8).contains($0.band) }
            .map { BandShift(band: $0.band, saturation: c($0.saturation, 100), lightness: c($0.lightness, 100)) }
        out.unrecognized = unrecognized
        if let a = cropAspect.flatMap(SmartCrop.parseAspect) {
            let ratio = a >= 1 ? a : 1 / a
            out.cropAspect = Self.cropAspects.min { abs(SmartCrop.parseAspect($0).map { $0 >= 1 ? $0 : 1 / $0 }! - ratio)
                < abs(SmartCrop.parseAspect($1).map { $0 >= 1 ? $0 : 1 / $0 }! - ratio) }
        }
        return out
    }
}

public enum NaturalLanguageEditError: Error, Equatable {
    case modelUnavailable(String)
}

/// Turns an instruction into an `EditIntent`.
public protocol NaturalLanguageEditService: Sendable {
    func intent(for text: String) async throws -> EditIntent
}

// The offline rule parser is ds-ai's `RuleBasedParser`, reached through the
// FFI (`DarkslideKit.RulesEditService`): one grammar, in Rust.

// MARK: - Foundation Models

#if canImport(FoundationModels)
/// Manual `Generable` conformance: the `@Generable` macro plugin ships only
/// with Xcode, and this builds with the Command Line Tools.
@available(macOS 26.0, *)
extension EditIntent: Generable {
    public static var generationSchema: GenerationSchema {
        GenerationSchema(type: Self.self, description: "Photo edit deltas. Use 0 for anything not requested.", properties: [
            .init(name: "temperatureShift", description: "White balance change in Kelvin; positive = warmer. A clear 'warmer' is about 800.",
                  type: Double.self, guides: [.range(-3000...3000)]),
            .init(name: "tintShift", description: "Tint change; positive = more magenta, negative = more green.",
                  type: Double.self, guides: [.range(-50...50)]),
            .init(name: "exposureShift", description: "Exposure change in stops (EV). 'Brighter' is about 0.5.",
                  type: Double.self, guides: [.range(-3...3)]),
            .init(name: "contrastShift", description: "Contrast change, -100 to 100.", type: Double.self, guides: [.range(-100...100)]),
            .init(name: "highlightsShift", description: "Highlights change; positive recovers (darkens) blown highlights.",
                  type: Double.self, guides: [.range(-100...100)]),
            .init(name: "shadowsShift", description: "Shadows change; positive lifts shadows.", type: Double.self,
                  guides: [.range(-100...100)]),
            .init(name: "saturationShift", description: "Saturation change; -100 = black and white.", type: Double.self,
                  guides: [.range(-100...100)]),
            .init(name: "cropAspect", description: "Requested crop aspect ratio, or 'none'.", type: String.self,
                  guides: [GenerationGuide<String>.anyOf(["none"] + cropAspects)]),
            .init(name: "straighten", description: "True if the user asks to level or straighten the horizon.", type: Bool.self),
            .init(name: "straightenDegrees",
                  description: "Rotation in degrees when the user gives a number (counter-clockwise positive), else 0.",
                  type: Double.self, guides: [.range(-45...45)]),
        ])
    }

    public init(_ content: GeneratedContent) throws {
        self.init(
            temperatureShift: try content.value(Double.self, forProperty: "temperatureShift"),
            tintShift: try content.value(Double.self, forProperty: "tintShift"),
            exposureShift: try content.value(Double.self, forProperty: "exposureShift"),
            contrastShift: try content.value(Double.self, forProperty: "contrastShift"),
            highlightsShift: try content.value(Double.self, forProperty: "highlightsShift"),
            shadowsShift: try content.value(Double.self, forProperty: "shadowsShift"),
            saturationShift: try content.value(Double.self, forProperty: "saturationShift"),
            cropAspect: try content.value(String?.self, forProperty: "cropAspect"),
            straighten: try content.value(Bool?.self, forProperty: "straighten") ?? false)
        let degrees = try content.value(Double?.self, forProperty: "straightenDegrees") ?? 0
        straightenDegrees = degrees == 0 ? nil : degrees
    }

    public var generatedContent: GeneratedContent {
        GeneratedContent(properties: [
            "temperatureShift": temperatureShift, "tintShift": tintShift, "exposureShift": exposureShift,
            "contrastShift": contrastShift, "highlightsShift": highlightsShift, "shadowsShift": shadowsShift,
            "saturationShift": saturationShift, "cropAspect": cropAspect ?? "none", "straighten": straighten,
            "straightenDegrees": straightenDegrees ?? 0,
        ])
    }
}
#endif

/// Apple's on-device LLM with guided generation into `EditIntent`.
public struct FoundationModelsEditService: NaturalLanguageEditService {
    public init() {}

    static let instructions = """
    You convert a photographer's editing request into numeric adjustment deltas for a RAW photo editor. \
    Only change what the request asks for; leave everything else at 0. Words like "much" roughly double a change, \
    "slightly" halves it.
    """

    /// Whether the system model can be used right now (Apple Intelligence on, model downloaded).
    public static var isAvailable: Bool {
        #if canImport(FoundationModels)
        if #available(macOS 26.0, *) { return SystemLanguageModel.default.isAvailable }
        #endif
        return false
    }

    public func intent(for text: String) async throws -> EditIntent {
        #if canImport(FoundationModels)
        if #available(macOS 26.0, *) {
            let model = SystemLanguageModel.default
            guard case .available = model.availability else {
                throw NaturalLanguageEditError.modelUnavailable(String(describing: model.availability))
            }
            let session = LanguageModelSession(model: model, instructions: Self.instructions)
            let response = try await session.respond(to: text, generating: EditIntent.self)
            return response.content.sanitized()
        }
        #endif
        throw NaturalLanguageEditError.modelUnavailable("FoundationModels not available")
    }
}

/// Tries `primary` (the LLM); on error or an empty result uses `fallback`
/// (the app passes ds-ai's rule parser).
public struct FallbackEditService: NaturalLanguageEditService {
    public let primary: any NaturalLanguageEditService
    public let fallback: any NaturalLanguageEditService

    public init(primary: any NaturalLanguageEditService = FoundationModelsEditService(),
                fallback: any NaturalLanguageEditService) {
        self.primary = primary
        self.fallback = fallback
    }

    public func intent(for text: String) async throws -> EditIntent {
        if let result = try? await primary.intent(for: text), !result.isEmpty { return result }
        try Task.checkCancellation()
        return try await fallback.intent(for: text)
    }
}
