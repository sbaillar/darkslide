import CoreGraphics
import DarkslideFFI
import Foundation

/// A parameter value of a plug-in step (JSON number, bool or string).
public enum PluginParamValue: Codable, Equatable, Hashable, Sendable {
    case number(Double)
    case bool(Bool)
    case text(String)

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let d = try? c.decode(Double.self) {
            self = .number(d)
        } else {
            self = .text(try c.decode(String.self))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case let .number(d): try c.encode(d)
        case let .bool(b): try c.encode(b)
        case let .text(s): try c.encode(s)
        }
    }

    /// From a param's `default_json`.
    init?(json: String) {
        guard let v = try? JSONDecoder().decode(PluginParamValue.self, from: Data(json.utf8)) else { return nil }
        self = v
    }

    var number: Double? { if case let .number(d) = self { d } else { nil } }
    var bool: Bool? { if case let .bool(b) = self { b } else { nil } }
    var text: String? { if case let .text(s) = self { s } else { nil } }
}

/// A plug-in post-process step of an export recipe (→ ds-ffi `ExportPostProcessStep`).
public struct RecipePostProcess: Codable, Equatable, Hashable, Sendable, Identifiable {
    public var pluginID: String
    public var contributionID: String
    /// Shown when the plug-in is gone ("Film Border").
    public var name: String
    /// Values the user changed, by param key; the others take the plug-in's defaults.
    public var values: [String: PluginParamValue]
    /// The plug-in version the step was set up with (nil in recipes saved before).
    public var pluginVersion: String?

    public var id: String { "\(pluginID)/\(contributionID)" }

    public init(_ info: PostProcessorInfo) {
        pluginID = info.pluginId
        contributionID = info.contributionId
        name = info.name
        values = [:]
        pluginVersion = info.pluginVersion
    }

    public var spec: ExportPostProcessStep {
        let json = (try? JSONEncoder().encode(values)).flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
        return ExportPostProcessStep(pluginId: pluginID, contributionId: contributionID, paramsJson: json,
                                     pluginVersion: pluginVersion)
    }

    public func matches(_ info: PostProcessorInfo) -> Bool {
        pluginID == info.pluginId && contributionID == info.contributionId
    }

    /// The value shown for `param`: the user's, else the default.
    public func value(_ param: PluginParamInfo) -> PluginParamValue? {
        values[param.key] ?? PluginParamValue(json: param.defaultJson)
    }

    public func number(_ param: PluginParamInfo) -> Double { value(param)?.number ?? param.min ?? 0 }
    public func bool(_ param: PluginParamInfo) -> Bool { value(param)?.bool ?? false }
    public func text(_ param: PluginParamInfo) -> String { value(param)?.text ?? "" }

    /// Set a number, clamped to the param's range (an int is rounded).
    public mutating func setNumber(_ v: Double, for param: PluginParamInfo) {
        var x = min(max(v, param.min ?? -.infinity), param.max ?? .infinity)
        if param.kind == "int" { x = x.rounded() }
        values[param.key] = .number(x)
    }

    public mutating func set(_ v: PluginParamValue, for param: PluginParamInfo) { values[param.key] = v }

    /// A colour param as a CGColor (sRGB, with its alpha).
    public func color(_ param: PluginParamInfo) -> CGColor {
        let rgba = Self.rgba(text(param)) ?? [0, 0, 0, 255]
        return CGColor(srgbRed: CGFloat(rgba[0]) / 255, green: CGFloat(rgba[1]) / 255, blue: CGFloat(rgba[2]) / 255,
                       alpha: CGFloat(rgba[3]) / 255)
    }

    public mutating func setColor(_ c: CGColor, for param: PluginParamInfo) {
        guard let srgb = CGColorSpace(name: CGColorSpace.sRGB),
              let comps = c.converted(to: srgb, intent: .defaultIntent, options: nil)?.components, comps.count >= 3
        else { return }
        let byte = { (v: CGFloat) in Int((min(max(v, 0), 1) * 255).rounded()) }
        let a = comps.count >= 4 ? byte(comps[3]) : 255
        let hex = String(format: "#%02X%02X%02X", byte(comps[0]), byte(comps[1]), byte(comps[2]))
        values[param.key] = .text(a == 255 ? hex : hex + String(format: "%02X", a))
    }

    /// `#RRGGBB` / `#RRGGBBAA` → RGBA bytes.
    static func rgba(_ s: String) -> [UInt8]? {
        guard s.hasPrefix("#"), s.count == 7 || s.count == 9 else { return nil }
        let hex = s.dropFirst()
        var out: [UInt8] = []
        var i = hex.startIndex
        while i < hex.endIndex {
            let j = hex.index(i, offsetBy: 2)
            guard let b = UInt8(hex[i..<j], radix: 16) else { return nil }
            out.append(b)
            i = j
        }
        return out.count == 3 ? out + [255] : out
    }
}

extension ExportRecipe {
    /// Whether the recipe runs `info`.
    public func runs(_ info: PostProcessorInfo) -> Bool { postProcess.contains { $0.matches(info) } }

    /// Turn a plug-in step on (appended last) or off.
    public mutating func setPostProcess(_ info: PostProcessorInfo, on: Bool) {
        postProcess.removeAll { $0.matches(info) }
        if on { postProcess.append(RecipePostProcess(info)) }
    }

    /// "Set up with Film Border 1.0.0; 1.1.0 is installed": the step's values may mean
    /// something else after an update (nil when the versions agree).
    public func versionNote(_ step: RecipePostProcess, available: [PostProcessorInfo]) -> String? {
        guard let old = step.pluginVersion, let info = available.first(where: step.matches),
              info.pluginVersion != old else { return nil }
        return "Set up with \(info.pluginName) \(old); \(info.pluginVersion) is installed. Check its settings."
    }

    /// Steps whose plug-in isn't active any more (the export would refuse them).
    public func missingPostProcess(available: [PostProcessorInfo]) -> [RecipePostProcess] {
        postProcess.filter { step in !available.contains(where: step.matches) }
    }
}

extension PostProcessorInfo {
    /// Unique across plug-ins (two may both have a `border` step).
    public var stepID: String { "\(pluginId)/\(contributionId)" }
}
