import CoreGraphics
import CoreImage
import DarkslideFFI
import Foundation
import ImageIO
import UniformTypeIdentifiers

/// ds-export's encoder seam (`ImageEncoder`, called from Rust export workers)
/// implemented with ImageIO: JPEG through ImageIO's (software) JPEG encoder and
/// HEIC through the media engine (HEVC). The ICC profile Rust built for the output
/// space is attached to the image, and the TIFF-structured EXIF blob is turned
/// into ImageIO property dictionaries (Exif/TIFF/GPS) so the file carries it.
/// It also decodes non-RAW originals (JPEG, HEIF, PNG, TIFF) for export, sized
/// and sharpened for a recipe on the GPU when Rust asks for a target.
public final class ImageIOEncoder: ImageEncoder, @unchecked Sendable {
    /// Test seam: fail every encode.
    var failWith: String?

    public init() {}

    public func encode(format: EncodedImageFormat, width: UInt32, height: UInt32, rgb16: Data, quality: UInt8,
                       icc: Data, exif: Data?, keywords: [String], metadata: PhotoMetadata, xmp: Data?) throws -> Data {
        if let failWith { throw DsError.Io(message: failWith) }
        let (w, h) = (Int(width), Int(height))
        guard w > 0, h > 0, rgb16.count == w * h * 6 else {
            throw DsError.InvalidArgument(message: "rgb16 buffer is \(rgb16.count) bytes for \(w)×\(h)")
        }
        // The pixels are already in this profile's encoding (an output profile from the
        // soft proof can be any RGB profile): tagging them sRGB instead would be wrong.
        guard let space = icc.isEmpty ? CGColorSpace(name: CGColorSpace.sRGB) : CGColorSpace(iccData: icc as CFData)
        else {
            throw DsError.InvalidArgument(message: "this ICC profile can't be embedded in a \(format == .jpeg ? "JPEG" : "HEIC") "
                + "file by ImageIO; export TIFF or PNG instead")
        }
        guard let provider = CGDataProvider(data: rgb16 as CFData),
              let image = CGImage(width: w, height: h, bitsPerComponent: 16, bitsPerPixel: 48, bytesPerRow: w * 6,
                                  space: space,
                                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.none.rawValue
                                      | Self.nativeByteOrder16.rawValue),
                                  provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)
        else { throw DsError.Internal(message: "can't make a \(w)×\(h) image") }
        let type = format == .jpeg ? UTType.jpeg : UTType.heic
        let out = NSMutableData()
        guard let dest = CGImageDestinationCreateWithData(out, type.identifier as CFString, 1, nil) else {
            throw DsError.Unsupported(message: "ImageIO can't write \(type.identifier)")
        }
        var props: [CFString: Any] = [
            kCGImageDestinationLossyCompressionQuality: Double(quality) / 100,
            kCGImagePropertyOrientation: 1,
        ]
        if let exif { props.merge(Self.metadataDictionaries(exifBlob: exif)) { _, new in new } }
        // Keywords and IPTC fields: ImageIO writes them as IPTC and XMP (dc:subject, dc:title…).
        if let iptc = Self.iptcDictionary(keywords: keywords, metadata: metadata) { props[kCGImagePropertyIPTCDictionary] = iptc }
        // Rust's XMP packet (rating, hierarchical tags) merged with the dictionaries above.
        if let xmp, let packet = CGImageMetadataCreateFromXMPData(xmp as CFData) {
            CGImageDestinationAddImageAndMetadata(dest, image, packet, props as CFDictionary)
        } else {
            CGImageDestinationAddImage(dest, image, props as CFDictionary)
        }
        guard CGImageDestinationFinalize(dest) else {
            throw DsError.Io(message: "ImageIO couldn't encode \(type.preferredFilenameExtension ?? "image")")
        }
        return out as Data
    }

    /// `encode` without an XMP packet (the IPTC dictionary only).
    public func encode(format: EncodedImageFormat, width: UInt32, height: UInt32, rgb16: Data, quality: UInt8,
                       icc: Data, exif: Data?, keywords: [String], metadata: PhotoMetadata) throws -> Data {
        try encode(format: format, width: width, height: height, rgb16: rgb16, quality: quality, icc: icc, exif: exif,
                   keywords: keywords, metadata: metadata, xmp: nil)
    }

    /// An original's image source, stored pixel size and EXIF orientation (header only).
    private static func open(_ path: String) throws -> (CGImageSource, Int, Int, CGImagePropertyOrientation, URL) {
        let url = URL(fileURLWithPath: path)
        guard let src = CGImageSourceCreateWithURL(url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary),
              let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any],
              let w0 = props[kCGImagePropertyPixelWidth] as? Int, let h0 = props[kCGImagePropertyPixelHeight] as? Int,
              w0 > 0, h0 > 0
        else { throw DsError.Io(message: "can't read \(url.lastPathComponent) as an image") }
        let o = (props[kCGImagePropertyOrientation] as? UInt32).flatMap(CGImagePropertyOrientation.init) ?? .up
        return (src, w0, h0, o, url)
    }

    /// The upright size of an original, from its header (read for a sized decode).
    public func uprightSize(path: String) throws -> ImageDimensions {
        // Without a GPU, or for an original past the GPU's texture limit or memory
        // budget (a phone panorama), Rust takes the full-size path (full decode, CPU resize).
        guard let resampler = ExportResampler.shared else { throw DsError.Unsupported(message: "no GPU") }
        let (_, w, h, o, _) = try Self.open(path)
        guard resampler.canResample(width: w, height: h) else {
            throw DsError.Unsupported(message: "\(w)×\(h) is too large to resize on the GPU")
        }
        let swap = [.leftMirrored, .right, .rightMirrored, .left].contains(o)
        return ImageDimensions(width: UInt32(swap ? h : w), height: UInt32(swap ? w : h))
    }

    /// Decode a non-RAW original (JPEG, HEIF, PNG, TIFF) for export (E-M2),
    /// upright (EXIF orientation applied), color-managed into `space`, as
    /// native-endian 16-bit RGB: at full size, or at `target` (gpu-audit #4:
    /// oriented, resized and sharpened on the GPU by `ExportResampler`, instead of a
    /// full-size copy across the FFI plus ds-export's CPU resize and sharpen).
    public func decode(path: String, space: ExportColorSpace, target: DecodeTarget?) throws -> DecodedImage {
        let (src, w0, h0, orientation, url) = try Self.open(path)
        if let target { return try Self.decodeSized(src, orientation, target, space, url) }
        // A "thumbnail" at the full long edge is ImageIO's way to get the upright image.
        let opts: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: max(w0, h0),
            kCGImageSourceShouldCacheImmediately: true,
        ]
        guard let image = CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary) else {
            throw DsError.Io(message: "can't decode \(url.lastPathComponent)")
        }
        let (w, h) = (image.width, image.height)
        let cs = CGColorSpace(name: space == .displayP3 ? CGColorSpace.displayP3 : CGColorSpace.sRGB)!
        // CoreGraphics has no 48-bit RGB context: draw into RGBX16, then drop X.
        guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 16, bytesPerRow: w * 8, space: cs,
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue | Self.nativeByteOrder16.rawValue),
              let base = ctx.data
        else { throw DsError.Internal(message: "can't make a \(w)×\(h) canvas") }
        ctx.interpolationQuality = .none
        ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
        let rgbx = base.bindMemory(to: UInt16.self, capacity: w * h * 4)
        var out = Data(count: w * h * 6)
        out.withUnsafeMutableBytes { raw in
            let d = raw.bindMemory(to: UInt16.self)
            for i in 0..<(w * h) {
                d[i * 3] = rgbx[i * 4]
                d[i * 3 + 1] = rgbx[i * 4 + 1]
                d[i * 3 + 2] = rgbx[i * 4 + 2]
            }
        }
        return DecodedImage(width: UInt32(w), height: UInt32(h), rgb16: out)
    }

    private static func decodeSized(_ src: CGImageSource, _ orientation: CGImagePropertyOrientation,
                                    _ target: DecodeTarget, _ space: ExportColorSpace, _ url: URL) throws -> DecodedImage {
        guard let resampler = ExportResampler.shared else { throw DsError.Unsupported(message: "no GPU") }
        guard target.width > 0, target.height > 0,
              let cg = CGImageSourceCreateImageAtIndex(src, 0, [kCGImageSourceShouldCacheImmediately: true] as CFDictionary)
        else { throw DsError.Io(message: "can't decode \(url.lastPathComponent)") }
        // Hand Core Image the decoded pixels: given the JPEG-backed image itself, it
        // re-decodes the file with its own YCbCr conversion (ΔE2000 ~0.9 from ImageIO's).
        guard let pixels = cg.dataProvider?.data, let provider = CGDataProvider(data: pixels),
              let tagged = cg.colorSpace,
              let rgb = CGImage(width: cg.width, height: cg.height, bitsPerComponent: cg.bitsPerComponent,
                                bitsPerPixel: cg.bitsPerPixel, bytesPerRow: cg.bytesPerRow, space: tagged,
                                bitmapInfo: cg.bitmapInfo, provider: provider, decode: nil, shouldInterpolate: false,
                                intent: .defaultIntent)
        else { throw DsError.Io(message: "can't decode \(url.lastPathComponent)") }
        var img = CIImage(cgImage: rgb).oriented(orientation)
        img = img.transformed(by: CGAffineTransform(translationX: -img.extent.minX, y: -img.extent.minY))
        let cs = CGColorSpace(name: space == .displayP3 ? CGColorSpace.displayP3 : CGColorSpace.sRGB)!
        let data = try resampler.resample(img, space: cs, width: Int(target.width), height: Int(target.height),
                                          sharpen: target.sharpen)
        return DecodedImage(width: target.width, height: target.height, rgb16: data)
    }

    static var nativeByteOrder16: CGBitmapInfo {
        UInt16(1).littleEndian == 1 ? .byteOrder16Little : .byteOrder16Big
    }

    // MARK: EXIF blob → ImageIO dictionaries

    /// A 1×1 JPEG (no metadata) used as a carrier: splice the APP1 Exif segment
    /// in and let ImageIO parse it into the same dictionaries it writes.
    private static let carrier: Data = {
        let one = CGContext(data: nil, width: 1, height: 1, bitsPerComponent: 8, bytesPerRow: 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!,
                            bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!.makeImage()!
        let d = NSMutableData()
        let dest = CGImageDestinationCreateWithData(d, UTType.jpeg.identifier as CFString, 1, nil)!
        CGImageDestinationAddImage(dest, one, nil)
        CGImageDestinationFinalize(dest)
        return d as Data
    }()

    /// ImageIO's IPTC dictionary for the keywords and the set fields; nil when there is nothing.
    static func iptcDictionary(keywords: [String], metadata m: PhotoMetadata) -> [CFString: Any]? {
        var d: [CFString: Any] = [:]
        if !keywords.isEmpty { d[kCGImagePropertyIPTCKeywords] = keywords }
        let fields: [(CFString, String)] = [
            (kCGImagePropertyIPTCObjectName, m.title), (kCGImagePropertyIPTCCaptionAbstract, m.caption),
            (kCGImagePropertyIPTCByline, m.creator), (kCGImagePropertyIPTCCopyrightNotice, m.copyright),
            (kCGImagePropertyIPTCCredit, m.credit), (kCGImagePropertyIPTCHeadline, m.headline),
            (kCGImagePropertyIPTCCity, m.city), (kCGImagePropertyIPTCProvinceState, m.state),
            (kCGImagePropertyIPTCCountryPrimaryLocationName, m.country), (kCGImagePropertyIPTCSubLocation, m.sublocation),
            (kCGImagePropertyIPTCSpecialInstructions, m.instructions), (kCGImagePropertyIPTCRightsUsageTerms, m.usageTerms),
        ]
        for (key, value) in fields where !value.isEmpty {
            // ImageIO takes the by-line as a list, like the keywords.
            d[key] = key == kCGImagePropertyIPTCByline ? [value] : value
        }
        return d.isEmpty ? nil : d
    }

    /// Exif/TIFF/GPS dictionaries of a TIFF-structured EXIF block (as ds-export
    /// writes into a JPEG APP1 after `Exif\0\0`). Orientation is dropped: the
    /// pixels are upright.
    static func metadataDictionaries(exifBlob: Data) -> [CFString: Any] {
        let payload = Data("Exif\0\0".utf8) + exifBlob
        guard payload.count + 2 <= 0xFFFF, carrier.count > 2 else { return [:] }
        var jpeg = Data([0xFF, 0xD8, 0xFF, 0xE1, UInt8((payload.count + 2) >> 8), UInt8((payload.count + 2) & 0xFF)])
        jpeg += payload
        jpeg += carrier.dropFirst(2)
        guard let src = CGImageSourceCreateWithData(jpeg as CFData, nil),
              let all = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any] else { return [:] }
        var out: [CFString: Any] = [:]
        for key in [kCGImagePropertyExifDictionary, kCGImagePropertyGPSDictionary, kCGImagePropertyExifAuxDictionary] {
            if let v = all[key] { out[key] = v }
        }
        if var tiff = all[kCGImagePropertyTIFFDictionary] as? [CFString: Any] {
            tiff[kCGImagePropertyTIFFOrientation] = 1
            out[kCGImagePropertyTIFFDictionary] = tiff
        }
        return out
    }
}
