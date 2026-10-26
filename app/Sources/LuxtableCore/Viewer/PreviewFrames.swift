import CoreGraphics
import Foundation
import Metal

/// 8-bit frames made from decoded preview images, shared so the viewer's
/// instant preview and the live renderer's frame of the same photo are one
/// allocation (N14). Byte-bounded LRU keyed by image identity.
///
/// Copies per displayed live preview (M3): ImageIO decodes the embedded JPEG
/// into the CGImage (1, shared with thumbnails); `convert` draws it once into a
/// shared-storage `MTLBuffer` with Metal-aligned rows (2); the viewer wraps that
/// buffer in a texture without copying (fit frames need no mips). Before: a
/// `Data` copy plus a `replace(region:)` texture upload (3 copies, 3 live buffers).
public final class PreviewFrames: @unchecked Sendable {
    public static let shared = PreviewFrames()

    private struct Entry {
        let image: CGImage // held so the identity key stays valid
        let frame: RenderedFrame
    }

    public let byteLimit: Int
    private let lock = NSLock()
    private var entries: [ObjectIdentifier: Entry] = [:]
    private var order: [ObjectIdentifier] = []
    private var bytes = 0
    private var _conversions = 0

    public init(byteLimit: Int = 128 << 20) { self.byteLimit = byteLimit }

    public var conversions: Int { lock.withLock { _conversions } }
    public var cachedBytes: Int { lock.withLock { bytes } }

    /// The frame for `img`, converting (off the caller's thread is the caller's job) once.
    public func frame(for img: CGImage) -> RenderedFrame? {
        let key = ObjectIdentifier(img)
        if let hit = lock.withLock({ () -> RenderedFrame? in
            guard let e = entries[key] else { return nil }
            order.removeAll { $0 == key }
            order.append(key)
            return e.frame
        }) { return hit }
        guard let f = Self.convert(img) else { return nil }
        lock.withLock {
            _conversions += 1
            if entries[key] == nil {
                entries[key] = Entry(image: img, frame: f)
                order.append(key)
                bytes += f.byteCount
            }
            while bytes > byteLimit, order.count > 1 {
                if let e = entries.removeValue(forKey: order.removeFirst()) { bytes -= e.frame.byteCount }
            }
        }
        return f
    }

    public func removeAll() {
        lock.withLock {
            entries.removeAll()
            order.removeAll()
            bytes = 0
        }
    }

    /// The device preview buffers are allocated on (nil: plain memory).
    static let device: MTLDevice? = MTLCreateSystemDefaultDevice()

    /// Row pitch Metal can wrap as a linear texture without copying.
    static func alignedRowBytes(width: Int, device: MTLDevice?) -> Int {
        let packed = width * 4
        guard let device else { return packed }
        let a = max(device.minimumLinearTextureAlignment(for: .rgba8Unorm_srgb), 4)
        return (packed + a - 1) / a * a
    }

    /// Draw a CGImage into an 8-bit sRGB frame backed by a shared `MTLBuffer`
    /// (heap memory without Metal).
    static func convert(_ img: CGImage, device: MTLDevice? = PreviewFrames.device) -> RenderedFrame? {
        let w = img.width, h = img.height
        guard w > 0, h > 0 else { return nil }
        let rowBytes = alignedRowBytes(width: w, device: device)
        guard let dest = try? FrameDestination(length: rowBytes * h, device: device),
              let ctx = CGContext(data: dest.pointer, width: w, height: h, bitsPerComponent: 8, bytesPerRow: rowBytes,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return nil }
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
        return dest.frame(RustRenderer.Geometry(width: w, height: h, bytesPerRow: rowBytes, format: .rgba8Srgb,
                                                region: nil, fullSize: CGSize(width: w, height: h)))
    }
}
