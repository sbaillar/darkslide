import LuxtableAI
import Foundation
import Metal

/// Vision mask bitmaps as `r8Unorm` textures for the viewer shader (L9 overlays
/// drawn on the GPU). One upload per mask; byte-capped LRU. A regenerated mask
/// (new pixel storage under the same key) is re-uploaded.
@MainActor
public final class MaskTextureCache {
    private struct Entry {
        let texture: MTLTexture
        let storage: UnsafeRawPointer?
        /// Keeps the uploaded storage alive so its address can't be reused by a newer mask.
        let pixels: [UInt8]
        var bytes: Int { texture.width * texture.height }
    }

    public let device: MTLDevice
    public let byteLimit: Int
    private var entries: [MaskOverlayCache.Key: Entry] = [:]
    private var order: [MaskOverlayCache.Key] = []
    public private(set) var uploads = 0

    public init(device: MTLDevice, byteLimit: Int = 32 << 20) {
        self.device = device
        self.byteLimit = byteLimit
    }

    public var cachedBytes: Int { entries.values.reduce(0) { $0 + $1.bytes } }

    public func texture(for key: MaskOverlayCache.Key, mask: VisionMask) -> MTLTexture? {
        let storage = mask.bytes.withUnsafeBytes { UnsafeRawPointer($0.baseAddress) }
        if let e = entries[key], e.storage == storage, e.texture.width == mask.width, e.texture.height == mask.height {
            order.removeAll { $0 == key }
            order.append(key)
            return e.texture
        }
        guard MetalLimits.canMakeTexture(device, width: mask.width, height: mask.height),
              mask.bytes.count >= mask.width * mask.height else { return nil }
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .r8Unorm, width: mask.width, height: mask.height,
                                                            mipmapped: false)
        desc.usage = .shaderRead
        desc.storageMode = .shared
        guard let tex = device.makeTexture(descriptor: desc) else { return nil }
        mask.bytes.withUnsafeBytes { raw in
            tex.replace(region: MTLRegionMake2D(0, 0, mask.width, mask.height), mipmapLevel: 0,
                        withBytes: raw.baseAddress!, bytesPerRow: mask.width)
        }
        uploads += 1
        entries[key] = Entry(texture: tex, storage: storage, pixels: mask.bytes)
        order.removeAll { $0 == key }
        order.append(key)
        while cachedBytes > byteLimit, order.count > 1 { entries[order.removeFirst()] = nil }
        return tex
    }
}
