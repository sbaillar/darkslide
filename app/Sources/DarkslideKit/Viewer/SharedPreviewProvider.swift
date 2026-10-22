import CoreGraphics
import Foundation

/// A preview still in its compressed form (the embedded JPEG) plus the RAW's orientation.
public struct EncodedPreview: Sendable {
    public let jpeg: Data
    public let orientation: UInt16
    public init(jpeg: Data, orientation: UInt16) {
        self.jpeg = jpeg
        self.orientation = orientation
    }
}

/// A preview provider that can hand out the compressed preview (extracted once).
public protocol EncodedPreviewSource: PreviewProvider {
    func encodedPreview(for url: URL) async throws -> EncodedPreview
}

/// Embedded previews shared by the viewer, the live renderer and thumbnails, in
/// a cache bounded by bytes (N14).
///
/// With an `EncodedPreviewSource` (the Rust extractor), each file's JPEG is
/// extracted **once** and kept compressed; every requested size is decoded
/// directly from it on the hardware JPEG path at that size (no CPU downscale),
/// orientation applied on the GPU. Other bases decode once at `canonicalEdge`
/// and smaller sizes are Lanczos-downscaled on the GPU (`GPUImaging`).
/// Concurrent requests for the same file share one in-flight extraction/decode.
public final class SharedPreviewProvider: PreviewProvider, @unchecked Sendable {
    private let base: any PreviewProvider
    public let canonicalEdge: Int
    public let byteLimit: Int
    private let lock = NSLock()
    private var entries: [String: CGImage] = [:]
    private var order: [String] = []
    private var bytes = 0
    private var inFlight: [URL: Task<CGImage, Error>] = [:]
    private var inFlightJPEG: [URL: Task<EncodedPreview, Error>] = [:]
    private var jpegs: [URL: EncodedPreview] = [:]
    private var jpegOrder: [URL] = []
    private var jpegBytes = 0
    private let imaging: GPUImaging
    private var _decodes = 0

    /// - Parameters:
    ///   - base: the provider that decodes from the RAW (e.g. `RustPreviewProvider(cacheLimit: 1)`).
    ///   - canonicalEdge: long edge of the single shared decode.
    ///   - byteLimit: total decoded bytes kept (default 192 MB ≈ 14 A9 previews + thumbnails).
    public init(base: any PreviewProvider, canonicalEdge: Int = 2048, byteLimit: Int = 192 << 20,
                imaging: GPUImaging = .shared) {
        self.base = base
        self.imaging = imaging
        self.canonicalEdge = max(64, canonicalEdge)
        self.byteLimit = max(1 << 20, byteLimit)
    }

    /// Decodes done by the base provider (each file once while it stays cached).
    public var decodes: Int { lock.withLock { _decodes } }
    /// Bytes currently cached.
    public var cachedBytes: Int { lock.withLock { bytes } }

    public func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
        if let source = base as? any EncodedPreviewSource { return try await decodedPreview(url, maxPixelSize, source) }
        let canonical = try await canonicalImage(url)
        let longEdge = max(canonical.width, canonical.height)
        guard maxPixelSize > 0, maxPixelSize < longEdge else { return canonical }
        let key = "\(url.path)@\(maxPixelSize)"
        if let hit = get(key) { return hit }
        guard let small = imaging.downscale(canonical, longEdge: maxPixelSize) else { return canonical }
        put(key, small)
        return small
    }

    private func canonicalImage(_ url: URL) async throws -> CGImage {
        let key = "\(url.path)@canonical"
        if let hit = get(key) { return hit }
        let task: Task<CGImage, Error> = lock.withLock {
            if let t = inFlight[url] { return t }
            _decodes += 1
            let base = base, edge = canonicalEdge
            let t = Task { try await base.preview(for: url, maxPixelSize: edge) }
            inFlight[url] = t
            return t
        }
        do {
            let img = try await task.value
            lock.withLock { inFlight[url] = nil }
            put(key, img)
            return img
        } catch {
            lock.withLock { inFlight[url] = nil }
            throw error
        }
    }

    private func get(_ key: String) -> CGImage? {
        lock.withLock {
            guard let img = entries[key] else { return nil }
            order.removeAll { $0 == key }
            order.append(key)
            return img
        }
    }

    private func put(_ key: String, _ img: CGImage) {
        let cost = img.bytesPerRow * img.height
        lock.withLock {
            if let old = entries[key] {
                bytes -= old.bytesPerRow * old.height
                order.removeAll { $0 == key }
            }
            entries[key] = img
            order.append(key)
            bytes += cost
            trimLocked()
        }
    }

    /// Hardware decode of the cached JPEG at the requested size (0 or ≥ canonical = canonical).
    private func decodedPreview(_ url: URL, _ maxPixelSize: Int, _ source: any EncodedPreviewSource) async throws -> CGImage {
        let size = maxPixelSize > 0 && maxPixelSize < canonicalEdge ? maxPixelSize : canonicalEdge
        let key = size == canonicalEdge ? "\(url.path)@canonical" : "\(url.path)@\(size)"
        if let hit = get(key) { return hit }
        let encoded = try await encodedJPEG(url, source)
        try Task.checkCancellation()
        guard let img = imaging.decode(jpeg: encoded.jpeg, maxPixelSize: size, exifOrientation: encoded.orientation) else {
            throw RenderError(.Unsupported(message: "the embedded preview of \(url.lastPathComponent) can't be decoded"))
        }
        put(key, img)
        return img
    }

    /// The compressed preview, extracted once per file and kept (it counts toward the byte cap).
    private func encodedJPEG(_ url: URL, _ source: any EncodedPreviewSource) async throws -> EncodedPreview {
        let task: Task<EncodedPreview, Error> = lock.withLock {
            if let e = jpegs[url] {
                jpegOrder.removeAll { $0 == url }
                jpegOrder.append(url)
                return Task { e }
            }
            if let t = inFlightJPEG[url] { return t }
            _decodes += 1
            let t = Task { try await source.encodedPreview(for: url) }
            inFlightJPEG[url] = t
            return t
        }
        do {
            let e = try await task.value
            lock.withLock {
                inFlightJPEG[url] = nil
                if jpegs[url] == nil {
                    jpegs[url] = e
                    jpegOrder.append(url)
                    jpegBytes += e.jpeg.count
                    bytes += e.jpeg.count
                    trimLocked()
                }
            }
            return e
        } catch {
            lock.withLock { inFlightJPEG[url] = nil }
            throw error
        }
    }

    /// Evict decoded images first (cheap to redo from the JPEG), then JPEGs. Lock held.
    private func trimLocked() {
        while bytes > byteLimit, order.count > 1 {
            let k = order.removeFirst()
            if let e = entries.removeValue(forKey: k) { bytes -= e.bytesPerRow * e.height }
        }
        while bytes > byteLimit, jpegOrder.count > 1 {
            let u = jpegOrder.removeFirst()
            if let e = jpegs.removeValue(forKey: u) {
                jpegBytes -= e.jpeg.count
                bytes -= e.jpeg.count
            }
        }
    }
}
