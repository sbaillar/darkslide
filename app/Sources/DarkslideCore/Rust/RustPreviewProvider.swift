import CoreGraphics
import DarkslideFFI
import Foundation
import ImageIO

/// Embedded JPEG previews from RAW files: extracted in Rust (≈5 ms, no sensor
/// decode), decoded with ImageIO and rotated upright.
///
/// Each file is decoded **once** at a fixed long edge and shared by the viewer,
/// the live renderer and thumbnails (smaller sizes are downscaled from that one
/// decode), in a cache bounded by bytes (N14). See `SharedPreviewProvider`.
public struct RustPreviewProvider: PreviewProvider {
    private let shared: SharedPreviewProvider

    /// - Parameters:
    ///   - byteLimit: decoded bytes kept (default 192 MB ≈ 14 A9 previews plus thumbnails).
    ///   - canonicalEdge: long edge of the single shared decode (the A9 preview is 1616).
    public init(byteLimit: Int = 192 << 20, canonicalEdge: Int = 2048) {
        shared = SharedPreviewProvider(base: RustPreviewDecoder(), canonicalEdge: canonicalEdge, byteLimit: byteLimit)
    }

    /// Compatibility: `cacheLimit` A9-sized previews (≈ 7 MB decoded each).
    public init(cacheLimit: Int) {
        self.init(byteLimit: max(1, cacheLimit) * (7 << 20))
    }

    /// Decodes performed (each file once while it stays cached).
    public var decodes: Int { shared.decodes }
    public var cachedBytes: Int { shared.cachedBytes }

    public func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
        try Task.checkCancellation() // a cancelled caller never starts a decode
        // [imp6] Imported clips show their poster frame (grid, viewer).
        if Photo.isVideo(url) { return try await VideoPoster.image(for: url, maxPixelSize: maxPixelSize) }
        return try await shared.preview(for: url, maxPixelSize: maxPixelSize)
    }

    /// Decode on the hardware JPEG path at `maxPixelSize` (0 = full) and apply the
    /// RAW's EXIF orientation on the GPU (see `GPUImaging`). `nil` if undecodable.
    static func decode(jpeg: Data, maxPixelSize: Int, exifOrientation: UInt16) -> CGImage? {
        GPUImaging.shared.decode(jpeg: jpeg, maxPixelSize: maxPixelSize, exifOrientation: exifOrientation)
    }
}

/// The uncached FFI + hardware decode behind `RustPreviewProvider`. Exposes the
/// compressed JPEG so the shared cache can keep it and decode every requested
/// size directly (no CPU downscale of a larger decode).
struct RustPreviewDecoder: EncodedPreviewSource {
    func encodedPreview(for url: URL) async throws -> EncodedPreview {
        let token = try CancelToken()
        let data: EmbeddedPreviewData
        do {
            data = try await withTaskCancellationHandler {
                try await embeddedPreview(path: url.path, token: token)
            } onCancel: {
                try? token.cancel()
            }
        } catch let e as DsError {
            if case .Cancelled = e { throw CancellationError() }
            throw RenderError(e)
        }
        try Task.checkCancellation()
        return EncodedPreview(jpeg: data.jpeg, orientation: data.orientation)
    }

    func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
        let e = try await encodedPreview(for: url)
        guard let image = RustPreviewProvider.decode(jpeg: e.jpeg, maxPixelSize: maxPixelSize,
                                                     exifOrientation: e.orientation) else {
            throw RenderError(.Unsupported(message: "the embedded preview of \(url.lastPathComponent) can't be decoded"))
        }
        return image
    }
}

/// Thread-safe LRU of decoded previews (deterministic eviction, unlike NSCache).
final class PreviewCache: @unchecked Sendable {
    private let lock = NSLock()
    private var order: [String] = []
    private var images: [String: CGImage] = [:]
    let limit: Int

    init(limit: Int) { self.limit = max(1, limit) }

    func get(_ key: String) -> CGImage? {
        lock.withLock {
            guard let img = images[key] else { return nil }
            order.removeAll { $0 == key }
            order.append(key)
            return img
        }
    }

    func set(_ key: String, _ img: CGImage) {
        lock.withLock {
            if images[key] == nil { order.append(key) }
            images[key] = img
            while order.count > limit { images[order.removeFirst()] = nil }
        }
    }

    var count: Int { lock.withLock { images.count } }
}

/// EXIF orientation (1…8) applied to pixels with Core Graphics.
enum ImageOrientation {
    /// Output size for an image of `w`×`h` stored pixels.
    static func orientedSize(exif: UInt16, width w: Int, height h: Int) -> (Int, Int) {
        (5...8).contains(exif) ? (h, w) : (w, h)
    }

    /// CG (y-up) transform taking the stored image rect to the upright output.
    static func transform(exif: UInt16, width w: CGFloat, height h: CGFloat) -> CGAffineTransform {
        switch exif {
        case 2: CGAffineTransform(a: -1, b: 0, c: 0, d: 1, tx: w, ty: 0)
        case 3: CGAffineTransform(a: -1, b: 0, c: 0, d: -1, tx: w, ty: h)
        case 4: CGAffineTransform(a: 1, b: 0, c: 0, d: -1, tx: 0, ty: h)
        case 5: CGAffineTransform(a: 0, b: -1, c: -1, d: 0, tx: h, ty: w)
        case 6: CGAffineTransform(a: 0, b: -1, c: 1, d: 0, tx: 0, ty: w)
        case 7: CGAffineTransform(a: 0, b: 1, c: 1, d: 0, tx: 0, ty: 0)
        case 8: CGAffineTransform(a: 0, b: 1, c: -1, d: 0, tx: h, ty: 0)
        default: .identity
        }
    }

    static func apply(exif: UInt16, to img: CGImage) -> CGImage? {
        guard (2...8).contains(exif) else { return img }
        let (ow, oh) = orientedSize(exif: exif, width: img.width, height: img.height)
        guard let ctx = CGContext(data: nil, width: ow, height: oh, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: img.colorSpace ?? CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return nil }
        ctx.concatenate(transform(exif: exif, width: CGFloat(img.width), height: CGFloat(img.height)))
        ctx.interpolationQuality = .none
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: img.width, height: img.height))
        return ctx.makeImage()
    }
}
