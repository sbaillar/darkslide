import CoreGraphics
import CryptoKit
import Foundation

/// A recipe's watermark as the app edits and saves it (→ ds-export through
/// `WatermarkSpec`): a line of text or a PNG logo, drawn on each output after it
/// is resized and sharpened, sized relative to the output.
public struct ExportWatermark: Codable, Equatable, Sendable {
    public enum Kind: String, Codable, CaseIterable, Sendable {
        case text, logo

        public var title: String { self == .text ? "Text" : "Logo" }
    }

    /// The 3×3 positions, row by row from the top left.
    public enum Anchor: String, Codable, CaseIterable, Sendable {
        case topLeft, top, topRight, left, center, right, bottomLeft, bottom, bottomRight

        public var title: String {
            switch self {
            case .topLeft: "Top Left"
            case .top: "Top"
            case .topRight: "Top Right"
            case .left: "Left"
            case .center: "Center"
            case .right: "Right"
            case .bottomLeft: "Bottom Left"
            case .bottom: "Bottom"
            case .bottomRight: "Bottom Right"
            }
        }
    }

    public var kind: Kind
    /// Tokens: `{copyright}` `{artist}` `{year}` `{filename}`.
    public var text: String
    /// Font family; empty = the system font.
    public var fontFamily: String
    /// Font size as a fraction of the output's short edge.
    public var textSize: Double
    /// sRGB `0xRRGGBB`.
    public var color: UInt32
    public var shadow: Bool
    /// The PNG logo (nil until one is chosen).
    public var logo: URL?
    /// Logo width as a fraction of the output's width.
    public var logoScale: Double
    public var anchor: Anchor
    /// Margin from the anchored edges, a fraction of the short edge.
    public var inset: Double
    /// 0…1.
    public var opacity: Double

    public init(kind: Kind = .text, text: String = "{copyright}", fontFamily: String = "", textSize: Double = 0.03,
                color: UInt32 = 0xFFFFFF, shadow: Bool = true, logo: URL? = nil, logoScale: Double = 0.15,
                anchor: Anchor = .bottomRight, inset: Double = 0.03, opacity: Double = 0.8) {
        self.kind = kind
        self.text = text
        self.fontFamily = fontFamily
        self.textSize = textSize
        self.color = color
        self.shadow = shadow
        self.logo = logo
        self.logoScale = logoScale
        self.anchor = anchor
        self.inset = inset
        self.opacity = opacity
    }

    /// Fields saved before they existed decode with their defaults.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = ExportWatermark()
        self.init(kind: try c.decodeIfPresent(Kind.self, forKey: .kind) ?? d.kind,
                  text: try c.decodeIfPresent(String.self, forKey: .text) ?? d.text,
                  fontFamily: try c.decodeIfPresent(String.self, forKey: .fontFamily) ?? d.fontFamily,
                  textSize: try c.decodeIfPresent(Double.self, forKey: .textSize) ?? d.textSize,
                  color: try c.decodeIfPresent(UInt32.self, forKey: .color) ?? d.color,
                  shadow: try c.decodeIfPresent(Bool.self, forKey: .shadow) ?? d.shadow,
                  logo: try c.decodeIfPresent(URL.self, forKey: .logo),
                  logoScale: try c.decodeIfPresent(Double.self, forKey: .logoScale) ?? d.logoScale,
                  anchor: try c.decodeIfPresent(Anchor.self, forKey: .anchor) ?? d.anchor,
                  inset: try c.decodeIfPresent(Double.self, forKey: .inset) ?? d.inset,
                  opacity: try c.decodeIfPresent(Double.self, forKey: .opacity) ?? d.opacity)
    }

    /// The colour for a colour well (sRGB); setting it converts to sRGB.
    public var cgColor: CGColor {
        get {
            CGColor(srgbRed: CGFloat((color >> 16) & 0xFF) / 255, green: CGFloat((color >> 8) & 0xFF) / 255,
                    blue: CGFloat(color & 0xFF) / 255, alpha: 1)
        }
        set {
            guard let srgb = CGColorSpace(name: CGColorSpace.sRGB),
                  let c = newValue.converted(to: srgb, intent: .defaultIntent, options: nil)?.components, c.count >= 3
            else { return }
            let byte = { (v: CGFloat) in UInt32((min(max(v, 0), 1) * 255).rounded()) }
            color = byte(c[0]) << 16 | byte(c[1]) << 8 | byte(c[2])
        }
    }

    /// "text watermark" / "logo watermark", for the recipe list's summary.
    public var summary: String {
        "\(kind == .text ? "text" : "logo") watermark"
    }
}

/// Watermark logos the user chose, copied into `<AppPaths.support>/Watermarks` so a
/// recipe keeps working when the original PNG is moved or deleted.
public enum WatermarkLogos {
    public static var folder: URL { AppPaths.support.appendingPathComponent("Watermarks", isDirectory: true) }

    public enum ImportError: Error, Equatable {
        case notPNG(String)
    }

    /// Copy `source` into `folder` and return the copy. Named by content
    /// (`<name>-<sha256 prefix>.png`): the same logo is stored once, and a different
    /// logo never replaces one a recipe uses. Written atomically (temp file, rename).
    public static func importLogo(_ source: URL, into folder: URL = folder) throws -> URL {
        let data = try Data(contentsOf: source)
        guard data.starts(with: [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) else {
            throw ImportError.notPNG(source.lastPathComponent)
        }
        let hash = SHA256.hash(data: data).prefix(6).map { String(format: "%02x", $0) }.joined()
        let dest = folder.appendingPathComponent("\(source.deletingPathExtension().lastPathComponent)-\(hash).png")
        if FileManager.default.fileExists(atPath: dest.path) { return dest }
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        try data.write(to: dest, options: .atomic)
        return dest
    }

    /// Delete the copies in `folder` that none of `recipes` uses any more. Only this
    /// folder's own PNGs are touched (never a user's original).
    public static func prune(keeping recipes: [ExportRecipe], in folder: URL = folder) {
        let used = Set(recipes.compactMap { $0.watermark?.logo?.standardizedFileURL.path })
        let files = (try? FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)) ?? []
        for f in files where f.pathExtension == "png" && !used.contains(f.standardizedFileURL.path) {
            try? FileManager.default.removeItem(at: f)
        }
    }
}
