import Foundation
import Metal

/// Uploads frames to Metal textures off the main thread and caches them by
/// frame id, so a redraw (pan, zoom, split drag, hover) never re-uploads.
///
/// - Uploads run on a serial background queue, never on the main thread.
/// - A Rust `MTLBuffer` frame that needs no mips becomes a zero-copy texture
///   over the buffer (M3); only minified frames (compare panes) get a mip chain,
///   which needs a private copy.
/// - Textures of frames no longer on screen are recycled into a pool bounded
///   by **bytes** with least-recently-used eviction (N3), and only after the
///   GPU finished the command buffer that last read them.
public final class TextureUploader: @unchecked Sendable {
    struct PoolKey: Hashable {
        let width: Int
        let height: Int
        let format: MTLPixelFormat
        let mips: Bool
    }

    struct Pooled {
        let key: PoolKey
        let texture: MTLTexture
        let bytes: Int
    }

    public let renderer: MetalViewerRenderer
    private let queue = DispatchQueue(label: "viewer.texture-upload", qos: .userInitiated)
    private let lock = NSLock()
    private var ready: [UInt64: MTLTexture] = [:]
    private var pending: Set<UInt64> = []
    /// LRU order, oldest first.
    private var pool: [Pooled] = []
    private var poolBytes = 0
    /// Total bytes the pool may hold.
    public let poolByteLimit: Int
    private var _uploads = 0
    private var _reuses = 0
    private var _zeroCopy = 0

    /// - Parameter poolByteLimit: bytes of idle textures kept for reuse (default 48 MB:
    ///   about two 2560 px frames). Everything else is freed.
    public init(renderer: MetalViewerRenderer, poolByteLimit: Int = 48 << 20) {
        self.renderer = renderer
        self.poolByteLimit = poolByteLimit
    }

    /// Uploads done, how many reused a pooled texture, and how many wrapped a buffer with no copy.
    public var stats: (uploads: Int, reuses: Int, zeroCopy: Int) { lock.withLock { (_uploads, _reuses, _zeroCopy) } }
    /// Bytes held by idle pooled textures.
    public var pooledBytes: Int { lock.withLock { poolBytes } }

    /// The texture for `frame` if uploaded; otherwise schedules the upload and
    /// returns nil. `onReady` runs (on the upload queue) when it becomes available.
    public func texture(for frame: RenderedFrame, mipmapped: Bool = false,
                        onReady: @escaping @Sendable () -> Void) -> MTLTexture? {
        let (existing, schedule) = lock.withLock { () -> (MTLTexture?, Bool) in
            if let t = ready[frame.id] { return (t, false) }
            if pending.contains(frame.id) { return (nil, false) }
            pending.insert(frame.id)
            return (nil, true)
        }
        if let existing { return existing }
        // A develop frame in a Metal buffer is wrapped right away (no copy, no hop): the
        // drag's new frame is drawn in the same pass it arrives in (no blank in between).
        if schedule, !mipmapped, let tex = renderer.zeroCopyTexture(for: frame) {
            lock.withLock {
                pending.remove(frame.id)
                ready[frame.id] = tex
                _uploads += 1
                _zeroCopy += 1
            }
            return tex
        }
        if schedule {
            queue.async { [self] in
                let tex = upload(frame, mipmapped: mipmapped)
                lock.withLock {
                    pending.remove(frame.id)
                    if let tex { ready[frame.id] = tex }
                }
                onReady()
            }
        }
        return nil
    }

    /// Synchronous variant (tests, snapshot): uploads on the calling thread if needed.
    public func textureNow(for frame: RenderedFrame, mipmapped: Bool = false) -> MTLTexture? {
        if let t = lock.withLock({ ready[frame.id] }) { return t }
        let tex = upload(frame, mipmapped: mipmapped)
        if let tex { lock.withLock { ready[frame.id] = tex } }
        return tex
    }

    /// Wait for queued uploads (tests).
    public func drain() { queue.sync {} }

    /// Keep only textures of `ids`. The rest are recycled into the pool once
    /// `commandBuffer` (the last one that may read them) completes; with no
    /// command buffer, immediately.
    public func retain(only ids: Set<UInt64>, after commandBuffer: MTLCommandBuffer? = nil) {
        let retired: [MTLTexture] = lock.withLock {
            var out: [MTLTexture] = []
            for (id, tex) in ready where !ids.contains(id) {
                ready[id] = nil
                out.append(tex)
            }
            return out
        }
        guard !retired.isEmpty else { return }
        if let commandBuffer {
            let box = RetiredTextures(textures: retired)
            commandBuffer.addCompletedHandler { [weak self] _ in self?.recycle(box.textures) }
        } else {
            recycle(retired)
        }
    }

    public var cachedIDs: Set<UInt64> { lock.withLock { Set(ready.keys) } }

    private func recycle(_ textures: [MTLTexture]) {
        lock.withLock {
            for tex in textures where tex.buffer == nil { // buffer-backed textures belong to their frame
                let key = PoolKey(width: tex.width, height: tex.height, format: tex.pixelFormat, mips: tex.mipmapLevelCount > 1)
                let cost = tex.allocatedSize
                guard cost <= poolByteLimit else { continue }
                pool.append(Pooled(key: key, texture: tex, bytes: cost))
                poolBytes += cost
            }
            while poolBytes > poolByteLimit, !pool.isEmpty {
                poolBytes -= pool.removeFirst().bytes
            }
        }
    }

    private func upload(_ frame: RenderedFrame, mipmapped: Bool) -> MTLTexture? {
        let key = PoolKey(width: frame.width, height: frame.height,
                          format: MetalViewerRenderer.textureFormat(for: frame.format), mips: mipmapped)
        if !mipmapped, let tex = renderer.zeroCopyTexture(for: frame) {
            lock.withLock {
                _uploads += 1
                _zeroCopy += 1
            }
            return tex
        }
        let reused: MTLTexture? = lock.withLock {
            _uploads += 1
            guard let i = pool.lastIndex(where: { $0.key == key }) else { return nil }
            let p = pool.remove(at: i)
            poolBytes -= p.bytes
            _reuses += 1
            return p.texture
        }
        if let reused {
            renderer.upload(frame, into: reused)
            return reused
        }
        return renderer.makeTexture(from: frame, mipmapped: mipmapped)
    }
}

/// Textures handed from the render thread to the GPU completion handler.
/// Safe: nothing uses them in between (they left `ready` under the lock).
private struct RetiredTextures: @unchecked Sendable {
    let textures: [MTLTexture]
}
