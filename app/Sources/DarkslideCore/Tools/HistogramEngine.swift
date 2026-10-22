import Foundation
import Metal
import simd

/// Histogram of a displayed frame, computed with a Metal compute kernel.
///
/// One dispatch bins R, G, B and Rec. 709 luma (same fixed-point weights as the
/// CPU reference) plus shadow/highlight clipping counts, reading the frame's
/// pixels in place (zero-copy over `MTLBuffer` frames). 16F linear-P3 frames are
/// converted to sRGB-encoded values in the kernel. The CPU binning in
/// `Histogram(frame:)` is only the fallback when Metal is unavailable or a
/// dispatch fails (tested).
public final class HistogramEngine: @unchecked Sendable {
    public static let shared = HistogramEngine()

    private let device: MTLDevice?
    private let queue: MTLCommandQueue?
    private let pipeline: MTLComputePipelineState?
    /// Test seam: make the GPU path fail so the CPU fallback runs.
    var failGPU = false
    private let lock = NSLock()
    private var _gpuRuns = 0
    private var _cpuRuns = 0

    /// Bins: 4 × 256 (R, G, B, L) + shadow + highlight.
    static let binCount = 4 * 256 + 2
    /// Pixels per thread along each axis (fewer device atomics than one per pixel).
    static let strip = 8

    static let source = """
    #include <metal_stdlib>
    using namespace metal;

    struct Params { uint width; uint height; uint isFloat; uint strip; float3x3 m; };

    static float encode(float v) {
        v = clamp(v, 0.0, 1.0);
        return v <= 0.0031308 ? v * 12.92 : 1.055 * pow(v, 1.0 / 2.4) - 0.055;
    }

    kernel void histogram(texture2d<float, access::read> tex [[texture(0)]],
                          device atomic_uint *bins [[buffer(0)]],
                          constant Params &p [[buffer(1)]],
                          uint2 gid [[thread_position_in_grid]],
                          uint tid [[thread_index_in_threadgroup]],
                          uint2 tsize [[threads_per_threadgroup]]) {
        uint tcount = tsize.x * tsize.y;
        threadgroup atomic_uint local[1026];
        for (uint i = tid; i < 1026; i += tcount) { atomic_store_explicit(&local[i], 0u, memory_order_relaxed); }
        threadgroup_barrier(mem_flags::mem_threadgroup);
        uint x0 = gid.x * p.strip, y0 = gid.y * p.strip;
        for (uint y = y0; y < min(y0 + p.strip, p.height); y++) {
            for (uint x = x0; x < min(x0 + p.strip, p.width); x++) {
                float3 c = tex.read(uint2(x, y)).rgb;
                if (p.isFloat != 0u) {
                    float3 l = p.m * c;
                    c = float3(encode(l.x), encode(l.y), encode(l.z));
                }
                uint r = uint(clamp(c.r, 0.0, 1.0) * 255.0 + 0.5);
                uint g = uint(clamp(c.g, 0.0, 1.0) * 255.0 + 0.5);
                uint b = uint(clamp(c.b, 0.0, 1.0) * 255.0 + 0.5);
                atomic_fetch_add_explicit(&local[r], 1u, memory_order_relaxed);
                atomic_fetch_add_explicit(&local[256 + g], 1u, memory_order_relaxed);
                atomic_fetch_add_explicit(&local[512 + b], 1u, memory_order_relaxed);
                atomic_fetch_add_explicit(&local[768 + ((54 * r + 183 * g + 19 * b) >> 8)], 1u, memory_order_relaxed);
                if (r == 0 || g == 0 || b == 0) { atomic_fetch_add_explicit(&local[1024], 1u, memory_order_relaxed); }
                if (r == 255 || g == 255 || b == 255) { atomic_fetch_add_explicit(&local[1025], 1u, memory_order_relaxed); }
            }
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
        for (uint i = tid; i < 1026; i += tcount) {
            uint v = atomic_load_explicit(&local[i], memory_order_relaxed);
            if (v != 0u) { atomic_fetch_add_explicit(&bins[i], v, memory_order_relaxed); }
        }
    }
    """

    struct Params {
        var width: UInt32
        var height: UInt32
        var isFloat: UInt32
        var strip: UInt32
        var m: simd_float3x3
    }

    public init(device: MTLDevice? = MTLCreateSystemDefaultDevice()) {
        self.device = device
        queue = device?.makeCommandQueue()
        pipeline = device.flatMap { d in
            (try? d.makeLibrary(source: Self.source, options: nil))
                .flatMap { $0.makeFunction(name: "histogram") }
                .flatMap { try? d.makeComputePipelineState(function: $0) }
        }
    }

    public var usesGPU: Bool { pipeline != nil && !failGPU }
    public var gpuRuns: Int { lock.withLock { _gpuRuns } }
    public var cpuRuns: Int { lock.withLock { _cpuRuns } }

    /// Histogram of `frame` (8-bit sRGB or 16F linear P3). Blocks until the GPU is
    /// done; call off the main thread.
    public func histogram(for frame: RenderedFrame) -> Histogram {
        if let h = gpuHistogram(frame) {
            lock.withLock { _gpuRuns += 1 }
            return h
        }
        lock.withLock { _cpuRuns += 1 }
        return Histogram(frame: ColorMath.sdrFrame(frame))
    }

    func gpuHistogram(_ frame: RenderedFrame) -> Histogram? {
        guard !failGPU, let device, let queue, let pipeline, frame.width > 0, frame.height > 0,
              let tex = texture(for: frame, device: device),
              let bins = device.makeBuffer(length: Self.binCount * 4, options: .storageModeShared),
              let cb = queue.makeCommandBuffer(), let enc = cb.makeComputeCommandEncoder() else { return nil }
        memset(bins.contents(), 0, bins.length)
        var p = Params(width: UInt32(frame.width), height: UInt32(frame.height),
                       isFloat: frame.format.isFloat ? 1 : 0, strip: UInt32(Self.strip),
                       m: frame.format == .rgba16fLinearRec2020 ? ViewerColor.rec2020ToSrgb : ViewerColor.p3ToSrgb)
        enc.setComputePipelineState(pipeline)
        enc.setTexture(tex, index: 0)
        enc.setBuffer(bins, offset: 0, index: 0)
        enc.setBytes(&p, length: MemoryLayout<Params>.stride, index: 1)
        let tg = MTLSize(width: 16, height: 16, depth: 1)
        let grid = MTLSize(width: (frame.width + Self.strip - 1) / Self.strip,
                           height: (frame.height + Self.strip - 1) / Self.strip, depth: 1)
        enc.dispatchThreads(grid, threadsPerThreadgroup: tg)
        enc.endEncoding()
        cb.commit()
        cb.waitUntilCompleted()
        guard cb.status == .completed else { return nil }
        let raw = bins.contents().bindMemory(to: UInt32.self, capacity: Self.binCount)
        let b = Array(UnsafeBufferPointer(start: raw, count: Self.binCount))
        return Histogram(red: Array(b[0..<256]), green: Array(b[256..<512]), blue: Array(b[512..<768]),
                         luma: Array(b[768..<1024]), shadowClipped: Int(b[1024]), highlightClipped: Int(b[1025]),
                         pixelCount: frame.width * frame.height)
    }

    /// A texture view of the frame's pixels: zero-copy over `MTLBuffer` frames
    /// (aligned rows), else one upload. 8-bit frames are read **encoded** (unorm,
    /// not _srgb) so bins match the stored values.
    func texture(for frame: RenderedFrame, device: MTLDevice) -> MTLTexture? {
        guard MetalLimits.canMakeTexture(device, width: frame.width, height: frame.height) else { return nil }
        let format: MTLPixelFormat = frame.format.isEightBit ? .rgba8Unorm : .rgba16Float
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: frame.width, height: frame.height,
                                                            mipmapped: false)
        desc.usage = .shaderRead
        if case .buffer(let buf) = frame.backing, buf.device === device,
           frame.bytesPerRow % device.minimumLinearTextureAlignment(for: format) == 0 {
            desc.storageMode = buf.storageMode
            return buf.makeTexture(descriptor: desc, offset: 0, bytesPerRow: frame.bytesPerRow)
        }
        desc.storageMode = .shared
        guard let tex = device.makeTexture(descriptor: desc) else { return nil }
        frame.withBytes { raw in
            tex.replace(region: MTLRegionMake2D(0, 0, frame.width, frame.height), mipmapLevel: 0,
                        withBytes: raw.baseAddress!, bytesPerRow: frame.bytesPerRow)
        }
        return tex
    }
}
