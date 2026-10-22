import CoreGraphics
import CoreImage
import DarkslideFFI
import Metal

/// gpu-audit #4: ds-export's resize and output sharpening on the GPU, for non-RAW
/// originals sized for a recipe. The same math as `ds_export::process` (separable
/// Catmull-Rom whose support widens with the downscale, in linear light; the
/// [1 2 1] unsharp mask on output-encoded values; 16-bit rounding between the two),
/// so a GPU-sized export matches the CPU path (ΔE2000 mean ≤ 0.1; see
/// `GpuAuditTests`). Core Image only orients and color-manages the decoded
/// original into a full-size 16-bit texture; three compute passes follow.
final class ExportResampler: @unchecked Sendable {
    /// `nil` without a Metal device (the caller then takes the CPU path).
    static let shared: ExportResampler? = MTLCreateSystemDefaultDevice().flatMap { try? ExportResampler(device: $0) }

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let ci: CIContext
    private let resizeV: MTLComputePipelineState
    private let resizeH: MTLComputePipelineState
    private let sharpen: MTLComputePipelineState

    private static let source = """
    #include <metal_stdlib>
    using namespace metal;

    inline float cr(float x) {
        x = fabs(x);
        if (x < 1.0) return 1.5 * x * x * x - 2.5 * x * x + 1.0;
        if (x < 2.0) return -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0;
        return 0.0;
    }
    inline float3 to_linear(float3 v) {
        return select(pow((v + 0.055) / 1.055, 2.4), v / 12.92, v <= 0.04045);
    }
    inline float3 to_encoded(float3 v) {
        v = clamp(v, 0.0, 1.0);
        return select(1.055 * pow(v, 1.0 / 2.4) - 0.055, v * 12.92, v <= 0.0031308);
    }

    // Vertical taps: encoded source -> linear (source width x output height).
    kernel void resize_v(texture2d<float, access::read> src [[texture(0)]],
                         texture2d<float, access::write> dst [[texture(1)]],
                         constant float &ratio [[buffer(0)]],
                         uint2 gid [[thread_position_in_grid]]) {
        if (gid.x >= dst.get_width() || gid.y >= dst.get_height()) return;
        float fscale = max(ratio, 1.0), center = (float(gid.y) + 0.5) * ratio;
        int lo = max(int(floor(center - 2.0 * fscale)), 0);
        int hi = min(int(ceil(center + 2.0 * fscale)), int(src.get_height()));
        float3 acc = 0.0; float sum = 0.0;
        for (int i = lo; i < hi; i++) {
            float w = cr((float(i) + 0.5 - center) / fscale);
            acc += w * to_linear(src.read(uint2(gid.x, uint(i))).rgb);
            sum += w;
        }
        dst.write(float4(fabs(sum) > 1.1920929e-7 ? acc / sum : acc, 1.0), gid);
    }

    // Horizontal taps: linear -> encoded output (16-bit texture: rounded like ds-export).
    kernel void resize_h(texture2d<float, access::read> src [[texture(0)]],
                         texture2d<float, access::write> dst [[texture(1)]],
                         constant float &ratio [[buffer(0)]],
                         uint2 gid [[thread_position_in_grid]]) {
        if (gid.x >= dst.get_width() || gid.y >= dst.get_height()) return;
        float fscale = max(ratio, 1.0), center = (float(gid.x) + 0.5) * ratio;
        int lo = max(int(floor(center - 2.0 * fscale)), 0);
        int hi = min(int(ceil(center + 2.0 * fscale)), int(src.get_width()));
        float3 acc = 0.0; float sum = 0.0;
        for (int i = lo; i < hi; i++) {
            float w = cr((float(i) + 0.5 - center) / fscale);
            acc += w * src.read(uint2(uint(i), gid.y)).rgb;
            sum += w;
        }
        dst.write(float4(to_encoded(fabs(sum) > 1.1920929e-7 ? acc / sum : acc), 1.0), gid);
    }

    // v + amount * (v - blur), blur = [1 2 1] x [1 2 1] / 16, edges clamped.
    kernel void sharpen(texture2d<float, access::read> src [[texture(0)]],
                        texture2d<float, access::write> dst [[texture(1)]],
                        constant float &amount [[buffer(0)]],
                        uint2 gid [[thread_position_in_grid]]) {
        uint w = src.get_width(), h = src.get_height();
        if (gid.x >= w || gid.y >= h) return;
        uint xs[3] = { gid.x > 0 ? gid.x - 1 : 0, gid.x, min(gid.x + 1, w - 1) };
        uint ys[3] = { gid.y > 0 ? gid.y - 1 : 0, gid.y, min(gid.y + 1, h - 1) };
        float k[3] = { 1.0, 2.0, 1.0 };
        float3 blur = 0.0;
        for (int j = 0; j < 3; j++)
            for (int i = 0; i < 3; i++)
                blur += k[j] * k[i] * src.read(uint2(xs[i], ys[j])).rgb;
        float3 v = src.read(gid).rgb;
        dst.write(float4(v + amount * (v - blur / 16.0), 1.0), gid);
    }
    """

    init(device: MTLDevice) throws {
        self.device = device
        guard let queue = device.makeCommandQueue() else { throw DsError.Internal(message: "no Metal queue") }
        self.queue = queue
        ci = CIContext(mtlCommandQueue: queue, options: [
            .workingColorSpace: CGColorSpace(name: CGColorSpace.extendedLinearSRGB)!, .workingFormat: CIFormat.RGBAf,
            .cacheIntermediates: false, .name: "export-resample",
        ])
        let lib = try device.makeLibrary(source: Self.source, options: nil)
        func pipeline(_ name: String) throws -> MTLComputePipelineState {
            try device.makeComputePipelineState(function: lib.makeFunction(name: name)!)
        }
        resizeV = try pipeline("resize_v")
        resizeH = try pipeline("resize_h")
        sharpen = try pipeline("sharpen")
    }

    /// Transient GPU memory one resample may take (full-size 16-bit source plus the
    /// float intermediate): a 50 MP original fits; larger ones take the CPU path.
    static let memoryBudget = 768 << 20

    /// Whether a `width`×`height` upright source can be resampled here: every texture
    /// within the device's limits (Metal aborts past them) and the memory budget.
    /// The output is never larger than the source.
    func canResample(width: Int, height: Int) -> Bool {
        MetalLimits.canMakeTexture(device, width: width, height: height)
            && width * height * 8 + width * height * 16 <= Self.memoryBudget
    }

    private func texture(_ w: Int, _ h: Int, _ format: MTLPixelFormat, shared: Bool = false) throws -> MTLTexture {
        guard MetalLimits.canMakeTexture(device, width: w, height: h) else {
            throw DsError.Unsupported(message: "\(w)×\(h) is beyond the GPU's texture limit")
        }
        let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: w, height: h, mipmapped: false)
        d.usage = [.shaderRead, .shaderWrite]
        d.storageMode = shared ? .shared : .private
        guard let t = device.makeTexture(descriptor: d) else {
            throw DsError.Internal(message: "can't allocate a \(w)×\(h) texture")
        }
        return t
    }

    private func dispatch(_ enc: MTLComputeCommandEncoder, _ p: MTLComputePipelineState, _ src: MTLTexture,
                          _ dst: MTLTexture, _ value: Float) {
        var v = value
        enc.setComputePipelineState(p)
        enc.setTexture(src, index: 0)
        enc.setTexture(dst, index: 1)
        enc.setBytes(&v, length: MemoryLayout<Float>.size, index: 0)
        enc.dispatchThreads(MTLSize(width: dst.width, height: dst.height, depth: 1),
                            threadsPerThreadgroup: MTLSize(width: 16, height: 16, depth: 1))
    }

    /// `image` (upright, origin at 0,0) color-managed into `space` at 16 bits,
    /// resized to `width`×`height` and sharpened by `sharpen`: interleaved RGB16,
    /// native byte order.
    func resample(_ image: CIImage, space: CGColorSpace, width: Int, height: Int, sharpen amount: Float) throws -> Data {
        let (sw, sh) = (Int(image.extent.width), Int(image.extent.height))
        guard canResample(width: sw, height: sh), width <= sw, height <= sh else {
            throw DsError.Unsupported(message: "\(sw)×\(sh) → \(width)×\(height) can't be resampled on the GPU")
        }
        let full = try texture(sw, sh, .rgba16Unorm)
        let mid = try texture(sw, height, .rgba32Float)
        let sized = try texture(width, height, .rgba16Unorm, shared: amount <= 0)
        let out = amount > 0 ? try texture(width, height, .rgba16Unorm, shared: true) : sized
        guard let cb = queue.makeCommandBuffer() else { throw DsError.Internal(message: "no Metal command buffer") }
        let dest = CIRenderDestination(mtlTexture: full, commandBuffer: cb)
        dest.colorSpace = space
        dest.isFlipped = true // texture row 0 = the image's top
        _ = try ci.startTask(toRender: image, from: image.extent, to: dest, at: .zero)
        guard let enc = cb.makeComputeCommandEncoder() else { throw DsError.Internal(message: "no Metal encoder") }
        dispatch(enc, resizeV, full, mid, Float(Double(sh) / Double(height)))
        dispatch(enc, resizeH, mid, sized, Float(Double(sw) / Double(width)))
        if amount > 0 { dispatch(enc, sharpen, sized, out, amount) }
        enc.endEncoding()
        cb.commit()
        cb.waitUntilCompleted()
        if let e = cb.error { throw DsError.Internal(message: "GPU resize failed: \(e.localizedDescription)") }
        var rgba = [UInt16](repeating: 0, count: width * height * 4)
        rgba.withUnsafeMutableBytes { raw in
            out.getBytes(raw.baseAddress!, bytesPerRow: width * 8,
                         from: MTLRegionMake2D(0, 0, width, height), mipmapLevel: 0)
        }
        var data = Data(count: width * height * 6)
        data.withUnsafeMutableBytes { raw in
            let d = raw.bindMemory(to: UInt16.self)
            for i in 0..<(width * height) {
                d[i * 3] = rgba[i * 4]
                d[i * 3 + 1] = rgba[i * 4 + 1]
                d[i * 3 + 2] = rgba[i * 4 + 2]
            }
        }
        return data
    }
}
