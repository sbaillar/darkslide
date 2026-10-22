import CoreGraphics
import CoreImage
import Foundation
import ImageIO
import Metal

/// Image work on Apple's accelerators, with a CPU fallback only where the
/// hardware path can fail.
///
/// - JPEG decode: ImageIO with `kCGImageSourceShouldCacheImmediately`. Small targets
///   are DCT-scaled in ImageIO's software decoder (cheaper than a full decode at this
///   size); larger ones are decoded in full and resized on the GPU (see `decode`).
/// - Orientation and Lanczos downscale: Core Image on a Metal-backed context
///   (GPU kernels), read back once as a `CGImage`.
/// - Fallback (Metal/CI unavailable or a render fails): Core Graphics on the CPU.
public final class GPUImaging: @unchecked Sendable {
    public static let shared = GPUImaging()

    /// Metal-backed Core Image context; nil when no GPU (then everything falls back).
    let context: CIContext?
    /// Test seam: force the GPU path to fail so the fallback runs.
    var failGPU = false
    /// Most pixels a full decode may allocate, and the longest edge it accepts. An embedded
    /// JPEG comes from an untrusted file and can claim 60000x60000 (14 GB decoded).
    static let maxDecodePixels = 250_000_000
    static let maxDecodeEdge = 32_768
    var maxPixels = GPUImaging.maxDecodePixels
    private let lock = NSLock()
    private var _gpuOps = 0
    private var _cpuFallbacks = 0

    public init(device: MTLDevice? = MTLCreateSystemDefaultDevice()) {
        context = device.map {
            CIContext(mtlDevice: $0, options: [.cacheIntermediates: false, .workingColorSpace: CGColorSpace(name: CGColorSpace.sRGB)!,
                                               .outputColorSpace: CGColorSpace(name: CGColorSpace.sRGB)!])
        }
    }

    /// Operations done on the GPU / on the CPU fallback (for tests and measurements).
    public var gpuOps: Int { lock.withLock { _gpuOps } }
    public var cpuFallbacks: Int { lock.withLock { _cpuFallbacks } }
    public var hasGPU: Bool { context != nil }

    // MARK: Decode

    /// Decode a JPEG to `maxPixelSize` (long edge; 0 = full size) and apply the EXIF
    /// orientation (1…8). The path depends on the size (measured on the A9's
    /// 1616×1080 embedded previews, median per image):
    ///
    /// - target ≤ half the source: ImageIO's software decoder scales in the DCT, and
    ///   the small result is oriented on the CPU; a GPU round trip costs more than it
    ///   saves at this size (256: 6.3 ms vs 7.5 ms, 512: 8.5 vs 11.6 ms);
    /// - larger targets and full size: a full ImageIO decode, then orientation and
    ///   Lanczos downscale in **one** Core Image pass on the GPU (1024: 11.5 ms vs
    ///   24.9 ms for ImageIO's own resample; full: 13.1 vs 15.8 ms with CPU orientation).
    ///   CPU fallback when the GPU pass fails. A source over `maxPixels` is only decoded
    ///   scaled (bounded memory); a full decode of it is refused (nil).
    public func decode(jpeg: Data, maxPixelSize: Int, exifOrientation: UInt16) -> CGImage? {
        guard let src = CGImageSourceCreateWithData(jpeg as CFData, [kCGImageSourceShouldCache: false] as CFDictionary)
        else { return nil }
        let long = Self.longEdge(of: src)
        if maxPixelSize > 0, long > 0, maxPixelSize * 2 <= long {
            let opts: [CFString: Any] = [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                // The RAW's orientation is authoritative; embedded JPEGs rarely carry one.
                kCGImageSourceCreateThumbnailWithTransform: false,
                kCGImageSourceShouldCacheImmediately: true,
                kCGImageSourceThumbnailMaxPixelSize: maxPixelSize,
            ]
            guard let small = CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary) else { return nil }
            return (2...8).contains(exifOrientation) ? ImageOrientation.apply(exif: exifOrientation, to: small) : small
        }
        let (w, h) = Self.dimensions(of: src)
        guard w <= Self.maxDecodeEdge, h <= Self.maxDecodeEdge, w * h <= maxPixels else { return nil }
        guard let full = CGImageSourceCreateImageAtIndex(src, 0, [kCGImageSourceShouldCacheImmediately: true] as CFDictionary)
        else { return nil }
        return orientAndFit(full, exif: exifOrientation, longEdge: maxPixelSize)
    }

    /// Long edge of the first image in `src` from its header (0 when unknown).
    static func longEdge(of src: CGImageSource) -> Int {
        let (w, h) = dimensions(of: src)
        return max(w, h)
    }

    /// Width and height of the first image in `src` from its header (0 when unknown).
    static func dimensions(of src: CGImageSource) -> (width: Int, height: Int) {
        let p = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any]
        return (p?[kCGImagePropertyPixelWidth] as? Int ?? 0, p?[kCGImagePropertyPixelHeight] as? Int ?? 0)
    }

    /// Orientation plus (when `longEdge` is smaller than the image) a Lanczos downscale,
    /// in one GPU pass; the CPU does both when the GPU pass fails.
    func orientAndFit(_ img: CGImage, exif: UInt16, longEdge: Int) -> CGImage? {
        let turns = (2...8).contains(exif)
        let long = max(img.width, img.height)
        let fit = longEdge > 0 && longEdge < long
        guard turns || fit else { return img }
        var ci = CIImage(cgImage: img)
        if turns { ci = ci.oriented(CGImagePropertyOrientation(rawValue: UInt32(exif))!) }
        let (ow, oh) = (Int(ci.extent.width), Int(ci.extent.height))
        var size = CGSize(width: ow, height: oh)
        if fit {
            let s = Double(longEdge) / Double(long)
            let w = max(1, Int((Double(ow) * s).rounded())), h = max(1, Int((Double(oh) * s).rounded()))
            ci = ci.applyingFilter("CILanczosScaleTransform", parameters: [
                kCIInputScaleKey: Double(h) / Double(oh),
                kCIInputAspectRatioKey: (Double(w) / Double(ow)) / (Double(h) / Double(oh)),
            ])
            size = CGSize(width: w, height: h)
        }
        if let out = gpuRender(ci, size: size) { return out }
        count(cpu: true)
        let s = Double(longEdge) / Double(long)
        let small = fit ? Self.cpuDownscale(img, width: max(1, Int((Double(img.width) * s).rounded())),
                                            height: max(1, Int((Double(img.height) * s).rounded()))) : img
        return turns ? small.flatMap { ImageOrientation.apply(exif: exif, to: $0) } : small
    }

    // MARK: Orientation

    /// Apply EXIF orientation. Identity (1 or unknown) returns the image untouched.
    public func orient(_ img: CGImage, exif: UInt16) -> CGImage? {
        guard (2...8).contains(exif) else { return img }
        if let out = gpuRender(CIImage(cgImage: img).oriented(CGImagePropertyOrientation(rawValue: UInt32(exif))!)) {
            return out
        }
        count(cpu: true)
        return ImageOrientation.apply(exif: exif, to: img)
    }

    // MARK: Downscale

    /// Lanczos downscale to `longEdge` on the GPU (never upscales).
    public func downscale(_ img: CGImage, longEdge: Int) -> CGImage? {
        let long = max(img.width, img.height)
        guard longEdge > 0, longEdge < long else { return img }
        let s = Double(longEdge) / Double(long)
        let w = max(1, Int((Double(img.width) * s).rounded())), h = max(1, Int((Double(img.height) * s).rounded()))
        let ci = CIImage(cgImage: img).applyingFilter("CILanczosScaleTransform", parameters: [
            kCIInputScaleKey: Double(h) / Double(img.height),
            kCIInputAspectRatioKey: (Double(w) / Double(img.width)) / (Double(h) / Double(img.height)),
        ])
        if let out = gpuRender(ci, size: CGSize(width: w, height: h)) { return out }
        count(cpu: true)
        return Self.cpuDownscale(img, width: w, height: h)
    }

    /// CPU fallback: Core Graphics high-quality resample.
    static func cpuDownscale(_ img: CGImage, width w: Int, height h: Int) -> CGImage? {
        guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return nil }
        ctx.interpolationQuality = .high
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
        return ctx.makeImage()
    }

    // MARK: -

    private func gpuRender(_ image: CIImage, size: CGSize? = nil) -> CGImage? {
        guard !failGPU, let context else { return nil }
        let extent = size.map { CGRect(origin: image.extent.origin, size: $0) } ?? image.extent
        guard let out = context.createCGImage(image, from: extent.integral, format: .RGBA8,
                                              colorSpace: CGColorSpace(name: CGColorSpace.sRGB)) else { return nil }
        count(cpu: false)
        return out
    }

    private func count(cpu: Bool) {
        lock.withLock { if cpu { _cpuFallbacks += 1 } else { _gpuOps += 1 } }
    }
}
