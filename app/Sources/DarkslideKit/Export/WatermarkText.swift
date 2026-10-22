import CoreGraphics
import CoreText
import DarkslideFFI
import Foundation

/// ds-export's text seam (`WatermarkTextRenderer`) with Core Text: one line in
/// the chosen family (any installed font, with the system's kerning, ligatures
/// and glyph fallback) as an 8-bit coverage mask. Colour, shadow, opacity and
/// placement are applied by ds-export, in the output's colour space.
/// Thread-safe: every call makes its own font and bitmap.
public final class CoreTextWatermarkRenderer: WatermarkTextRenderer, @unchecked Sendable {
    public init() {}

    /// The font for `family` at `size` px per em; empty or unknown = the system font.
    /// Whether `family` is an installed font family (case-insensitive).
    public static func isInstalled(_ family: String) -> Bool {
        let got = CTFontCopyFamilyName(font(family: family, size: 12)) as String
        return got.caseInsensitiveCompare(family) == .orderedSame
    }

    static func font(family: String, size: CGFloat) -> CTFont {
        let system = CTFontCreateUIFontForLanguage(.system, size, nil)
            ?? CTFontCreateWithName("Helvetica" as CFString, size, nil)
        guard !family.isEmpty else { return system }
        let desc = CTFontDescriptorCreateWithAttributes([kCTFontFamilyNameAttribute: family] as CFDictionary)
        let font = CTFontCreateWithFontDescriptor(desc, size, nil)
        // Core Text substitutes a default for an unknown family: use the system font then.
        let got = CTFontCopyFamilyName(font) as String
        return got.caseInsensitiveCompare(family) == .orderedSame ? font : system
    }

    public func rasterize(text: String, font: String, pixelSize: Float) throws -> WatermarkTextMask {
        guard pixelSize.isFinite, pixelSize > 0, pixelSize < 20_000 else {
            throw DsError.InvalidArgument(message: "text size \(pixelSize) px")
        }
        let ctFont = Self.font(family: font, size: CGFloat(pixelSize))
        let attr = NSAttributedString(string: text, attributes: [
            NSAttributedString.Key(kCTFontAttributeName as String): ctFont,
            NSAttributedString.Key(kCTForegroundColorFromContextAttributeName as String): true,
        ])
        let line = CTLineCreateWithAttributedString(attr)
        var ascent: CGFloat = 0, descent: CGFloat = 0, leading: CGFloat = 0
        let advance = CTLineGetTypographicBounds(line, &ascent, &descent, &leading)
        // The typographic box, grown to the glyphs' ink: italic overhangs, accents above
        // the ascent and script swashes are kept instead of clipped.
        var box = CGRect(x: 0, y: -descent, width: advance, height: ascent + descent)
        let measure = CGContext(data: nil, width: 1, height: 1, bitsPerComponent: 8, bytesPerRow: 1,
                                space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue)
        let ink = measure.map { CTLineGetImageBounds(line, $0) } ?? .null
        if !ink.isNull, !ink.isEmpty { box = box.union(ink) }
        box = box.integral
        let w = Int(box.width), h = Int(box.height)
        guard w > 0, h > 0, w * h < 1 << 28 else {
            throw DsError.InvalidArgument(message: "text \"\(text)\" has no size at \(pixelSize) px")
        }
        // Grey, no alpha: white ink on black, so each byte is the coverage.
        guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w,
                                  space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue)
        else { throw DsError.Internal(message: "can't make a \(w)×\(h) text bitmap") }
        ctx.setShouldSmoothFonts(false)
        ctx.setFillColor(gray: 1, alpha: 1)
        ctx.textPosition = CGPoint(x: -box.minX, y: -box.minY)
        CTLineDraw(line, ctx)
        guard let data = ctx.data else { throw DsError.Internal(message: "text bitmap has no pixels") }
        // Bitmap memory starts with the top row, as ds-export expects.
        let coverage = Data(bytes: data, count: w * h)
        return WatermarkTextMask(width: UInt32(w), height: UInt32(h), coverage: coverage)
    }
}

/// The recipe editor's watermark preview: an image ↔ the RGBA8 buffer
/// `watermark_preview` draws on.
enum WatermarkPreview {
    /// Under the preview when the text is blank for the preview photo.
    static let blankMessage = "The watermark text is empty for this photo (no copyright or artist?), "
        + "so its export will be left without a watermark."

    /// `image` as sRGB RGBA8 (premultiplied-last; opaque photos are unaffected),
    /// at most `maxPixelSize` on the long edge.
    static func pixels(_ image: CGImage, maxPixelSize: Int = 1024) -> WatermarkPreviewPixels? {
        rgba(image, maxPixelSize: maxPixelSize).map { WatermarkPreviewPixels(width: $0.width, height: $0.height, rgba: $0.data) }
    }

    static func rgba(_ image: CGImage, maxPixelSize: Int = 1024) -> (width: Int, height: Int, data: Data)? {
        let scale = min(1, Double(maxPixelSize) / Double(max(image.width, image.height)))
        let w = max(1, Int((Double(image.width) * scale).rounded()))
        let h = max(1, Int((Double(image.height) * scale).rounded()))
        guard let srgb = CGColorSpace(name: CGColorSpace.sRGB),
              let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: srgb,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return nil }
        ctx.interpolationQuality = .high
        ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
        guard let p = ctx.data else { return nil }
        return (w, h, Data(bytes: p, count: w * h * 4))
    }

    /// An sRGB image from RGBA8 bytes.
    static func image(width: Int, height: Int, rgba: Data) -> CGImage? {
        guard rgba.count == width * height * 4, let srgb = CGColorSpace(name: CGColorSpace.sRGB),
              let provider = CGDataProvider(data: rgba as CFData)
        else { return nil }
        return CGImage(width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4,
                       space: srgb, bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                       provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
    }

    /// The FFI spec of a watermark (a logo not chosen yet is an empty path, which
    /// ds-export reports).
    static func spec(_ w: ExportWatermark) -> WatermarkSpec {
        let content: WatermarkContent = switch w.kind {
        case .text: .text(text: w.text, font: w.fontFamily, size: Float(w.textSize), color: w.color, shadow: w.shadow)
        case .logo: .logo(path: w.logo?.path ?? "", scale: Float(w.logoScale))
        }
        let anchor: WatermarkAnchor = switch w.anchor {
        case .topLeft: .topLeft
        case .top: .top
        case .topRight: .topRight
        case .left: .left
        case .center: .center
        case .right: .right
        case .bottomLeft: .bottomLeft
        case .bottom: .bottom
        case .bottomRight: .bottomRight
        }
        return WatermarkSpec(content: content, anchor: anchor, inset: Float(w.inset), opacity: Float(w.opacity))
    }
}
