import CoreGraphics
import Foundation
import Metal
import simd

/// Draws image panes into a Metal pass: before/after split, tiles, a loupe,
/// clipping warning and focus peaking, the display transform (quarter turns and
/// straighten), with color conversion into the target space (sRGB SDR or
/// extended-linear Display P3 for EDR).
///
/// Shaders are compiled at runtime from source (`makeLibrary(source:)`), so the
/// app builds with only the Command Line Tools (no `metal` compiler). Zoom and
/// pan only change uniforms: no re-render or re-upload on pan.
public final class MetalViewerRenderer: @unchecked Sendable {
    public let device: MTLDevice
    /// Default target format (used by `renderOffscreen` and when none is given).
    public let pixelFormat: MTLPixelFormat
    private let queue: MTLCommandQueue
    private let library: MTLLibrary
    private let lock = NSLock()
    private var pipelines: [MTLPixelFormat: MTLRenderPipelineState] = [:]
    private let smoothSampler: MTLSamplerState
    private let nearestSampler: MTLSamplerState
    /// Bound to the proof slot when a pane has no proof table (never sampled then).
    private let noProofLUT: MTLTexture

    /// One textured quad.
    ///
    /// The quad's local 0...1 maps to `span`, a rect in **display** space
    /// (normalized, after quarter turns and straighten). Each fragment goes
    /// through the inverse view transform (angle, then quarter turns) into
    /// **oriented** space, then into the texture via `region` (the oriented part
    /// of the image the texture holds). Fragments outside the texture are
    /// discarded, so tiles and rotated images need no special geometry.
    /// Pixels left of `split` (NDC x) show `before`.
    public struct Pane {
        public var after: MTLTexture
        public var before: MTLTexture?
        public var rect: SIMD4<Float>
        public var split: Float
        public var clip: SIMD4<Float>
        /// Display-space rect (x0, y0, x1, y1) the quad covers.
        public var span: SIMD4<Float> = SIMD4(0, 0, 1, 1)
        /// Oriented-space rect (x0, y0, x1, y1) the texture holds.
        public var region: SIMD4<Float> = SIMD4(0, 0, 1, 1)
        /// Clockwise quarter turns 0...3.
        public var quarterTurns: UInt32 = 0
        /// Straighten, radians, clockwise on screen.
        public var angle: Float = 0
        /// Width/height of the displayed (turned) image.
        public var aspect: Float = 1
        /// Focus-peaking filter step in display-normalized units (≈ 1 device pixel),
        /// the same for the fit frame and tiles so there is no seam at a tile edge.
        /// Nil: one texel of `after` (whole-image quads).
        public var peakStep: SIMD2<Float>?
        /// Sample with nearest filtering (zoomed past 1:1) instead of trilinear.
        public var nearest = false
        /// Clip the quad to an inscribed circle with a ring (the loupe).
        public var circle = false
        public var clippingWarning = false
        public var focusPeaking = false
        /// Converts the texture's (linear) color space into the target's.
        public var colorMatrix = matrix_identity_float3x3
        /// Vision mask bitmaps (r8, oriented image space) tinted over the image (≤ 4).
        public var masks: [MTLTexture] = []
        /// Mask tint in linear sRGB (rgb) and strength (a).
        public var maskTint = SIMD4<Float>(1, 0.1, 0.1, 0.35)
        /// Brush strokes (oriented space) drawn as round-capped segments.
        public var segments: [OverlaySegment] = []
        /// The texture is a developed frame of the final image: display space itself
        /// (`region` is display space; no turn/straighten on the way to the texture).
        public var displaySpace = false
        /// Display -> crop-space affine (rows `(m00, m01, m02)`, `(m10, m11, m12)`) for
        /// developed display: oriented textures (embedded previews), masks and strokes
        /// are placed through it instead of the turn/straighten transform.
        public var cropAffine: (SIMD3<Float>, SIMD3<Float>)?
        /// Width/height of crop space in pixels (brush strokes are round there).
        public var cropAspect: Float?
        /// Soft proof: the proof table (3D, see `ProofTable`), nil when off.
        public var proofLUT: MTLTexture?
        /// The texture's linear colour → linear Rec.2020 (the table's input primaries).
        public var proofIn = matrix_identity_float3x3
        /// Linear Rec.2020 (the table's output) → the target's primaries.
        public var proofOut = matrix_identity_float3x3
        /// Paint colours outside the proof profile's gamut with `gamutColor` (linear, in
        /// the target's primaries).
        public var gamutWarning = false
        public var gamutColor = SIMD3<Float>(0.5, 0.5, 0.5)
        /// Gray proof profiles: out of gamut is chroma (max − min of the encoded
        /// Rec.2020 colour) above this, not the table's alpha. 0: use alpha.
        public var neutralTolerance: Float = 0

        public init(after: MTLTexture, before: MTLTexture? = nil, rect: SIMD4<Float>, split: Float = -2,
                    clip: SIMD4<Float> = SIMD4(-1, -1, 1, 1)) {
            self.after = after
            self.before = before
            self.rect = rect
            self.split = split
            self.clip = clip
        }
    }

    public enum SetupError: Error { case noDevice, noQueue, shader(String) }

    /// Matches `Uniforms` in the shader.
    struct Uniforms {
        var m: simd_float3x3
        var rect: SIMD4<Float>
        var clip: SIMD4<Float>
        var span: SIMD4<Float>
        var region: SIMD4<Float>
        var split: Float
        var lineHalfWidth: Float
        var peakThreshold: Float
        var flags: UInt32
        var peakStep: SIMD2<Float>
        var clipHigh: Float
        var angle: Float
        var aspect: Float
        var turns: UInt32
        var pad = SIMD2<Float>(0, 0)
        var maskTint = SIMD4<Float>(0, 0, 0, 0)
        var segmentCount: UInt32 = 0
        var maskCount: UInt32 = 0
        var orientedAspect: Float = 1
        var pad2: Float = 0
        var aff0 = SIMD4<Float>(1, 0, 0, 0)
        var aff1 = SIMD4<Float>(0, 1, 0, 0)
        var proofIn = matrix_identity_float3x3
        var proofOut = matrix_identity_float3x3
        var gamutColor = SIMD4<Float>(0.5, 0.5, 0.5, 1)
    }

    /// Longest stroke list sent to the shader per pane (decimated beyond).
    public static let maxSegments = 512

    enum Flag {
        static let clipping: UInt32 = 1
        static let peaking: UInt32 = 2
        static let circle: UInt32 = 4
        static let nearest: UInt32 = 8
        static let displaySpace: UInt32 = 16
        static let cropAffine: UInt32 = 32
        static let proof: UInt32 = 64
        static let gamutWarning: UInt32 = 128
    }

    /// Highlight clipping threshold per channel: SDR white (N12). 8-bit frames
    /// top out at exactly 1.0, so a hair below catches 255; float frames clip at 1.0.
    static func clipHigh(isFloat: Bool) -> Float { isFloat ? 1.0 : 0.998 }

    /// Peaking threshold on contrast-normalized gamma-luma gradients (N13).
    static let peakThreshold: Float = 0.8

    static let shaderSource = """
    #include <metal_stdlib>
    using namespace metal;

    struct Uniforms {
        float3x3 m; float4 rect; float4 clip; float4 span; float4 region;
        float split; float lineHalfWidth; float peakThreshold; uint flags;
        float2 peakStep; float clipHigh; float angle; float aspect; uint turns; float2 pad;
        float4 maskTint; uint segmentCount; uint maskCount; float orientedAspect; float pad2;
        float4 aff0; float4 aff1;
        float3x3 proofIn; float3x3 proofOut; float4 gamutColor;
    };
    struct Segment { float4 ab; float radius; uint kind; float2 pad; };
    struct VOut { float4 pos [[position]]; float2 ndc; float2 local; };

    vertex VOut viewer_vertex(uint vid [[vertex_id]], constant Uniforms &u [[buffer(0)]]) {
        float2 c = float2(vid & 1, vid >> 1);
        float2 p = mix(u.rect.xy, u.rect.zw, c);
        VOut o;
        o.pos = float4(p, 0, 1);
        o.ndc = p;
        o.local = float2(c.x, 1 - c.y);
        return o;
    }

    // Display-normalized -> oriented-normalized (inverse of: turn, then rotate clockwise).
    static float2 toOriented(float2 t, constant Uniforms &u) {
        float2 p = (t - 0.5) * float2(u.aspect, 1.0);
        float c = cos(u.angle), s = sin(u.angle);
        p = float2(c * p.x + s * p.y, -s * p.x + c * p.y);
        float2 q = p / float2(u.aspect, 1.0) + 0.5;
        if (u.turns == 1u) { return float2(q.y, 1.0 - q.x); }
        if (u.turns == 2u) { return float2(1.0 - q.x, 1.0 - q.y); }
        if (u.turns == 3u) { return float2(1.0 - q.y, q.x); }
        return q;
    }

    // Display-normalized -> crop space: the developed frame's affine, else the turn/straighten.
    static float2 toCrop(float2 t, constant Uniforms &u) {
        if ((u.flags & 32u) != 0u) {
            return float2(dot(u.aff0.xy, t) + u.aff0.z, dot(u.aff1.xy, t) + u.aff1.z);
        }
        return toOriented(t, u);
    }

    static float2 texCoord(float2 t, constant Uniforms &u) {
        float2 o = (u.flags & 16u) != 0u ? t : toCrop(t, u);
        return (o - u.region.xy) / (u.region.zw - u.region.xy);
    }

    // Perceptual (gamma) luma of linear RGB.
    static float gluma(float3 c) { return pow(max(dot(c, float3(0.2126, 0.7152, 0.0722)), 0.0), 1.0 / 2.2); }

    fragment float4 viewer_fragment(VOut in [[stage_in]], constant Uniforms &u [[buffer(0)]],
                                    texture2d<float> after [[texture(0)]],
                                    texture2d<float> before [[texture(1)]],
                                    texture2d<float> mask0 [[texture(2)]], texture2d<float> mask1 [[texture(3)]],
                                    texture2d<float> mask2 [[texture(4)]], texture2d<float> mask3 [[texture(5)]],
                                    texture3d<float> proofLUT [[texture(6)]],
                                    constant Segment *segs [[buffer(1)]],
                                    sampler smooth [[sampler(0)]], sampler nearest [[sampler(1)]]) {
        if (in.ndc.x < u.clip.x || in.ndc.x > u.clip.z || in.ndc.y < u.clip.y || in.ndc.y > u.clip.w) {
            discard_fragment();
        }
        if ((u.flags & 4u) != 0u) {
            float d = length(in.local - 0.5);
            if (d > 0.5) { discard_fragment(); }
            if (d > 0.485) { return float4(1, 1, 1, 1); }
        }
        if (abs(in.ndc.x - u.split) < u.lineHalfWidth) {
            return float4(1, 1, 1, 1);
        }
        float2 t = mix(u.span.xy, u.span.zw, in.local);
        float2 tc = texCoord(t, u);
        if (any(tc < 0.0) || any(tc > 1.0)) { discard_fragment(); }
        bool useNearest = (u.flags & 8u) != 0u;
        texture2d<float> src = in.ndc.x < u.split ? before : after;
        float4 c = useNearest ? src.sample(nearest, tc) : src.sample(smooth, tc);
        float3 rgb = u.m * c.rgb;
        if ((u.flags & 64u) != 0u) {
            // Soft proof: the table maps sRGB-encoded Rec.2020 (the working gamut) to the
            // proofed colour (linear Rec.2020) and the signed distance to the proof
            // profile's gamut surface (alpha > 0: outside).
            float3 p = clamp(u.proofIn * c.rgb, 0.0, 1.0);
            float3 e = select(1.055 * pow(p, 1.0 / 2.4) - 0.055, p * 12.92, p <= 0.0031308);
            float n = float(proofLUT.get_width());
            float4 s = proofLUT.sample(smooth, e * ((n - 1.0) / n) + 0.5 / n);
            rgb = u.proofOut * s.rgb;
            // gamutColor.a > 0: a gray profile, whose gamut (the neutral axis) is a chroma test.
            float chroma = max(e.x, max(e.y, e.z)) - min(e.x, min(e.y, e.z));
            bool outside = u.gamutColor.a > 0.0 ? chroma > u.gamutColor.a : s.a > 0.0;
            if ((u.flags & 128u) != 0u && outside) { rgb = u.gamutColor.rgb; }
        }
        if ((u.flags & 2u) != 0u) {
            float2 k = u.peakStep;
            float v[9];
            float sum = 0.0;
            for (int j = -1; j <= 1; j++) {
                for (int i = -1; i <= 1; i++) {
                    float2 tt = clamp(texCoord(t + float2(i, j) * k, u), 0.0, 1.0);
                    float l = gluma(src.sample(nearest, tt).rgb);
                    v[(j + 1) * 3 + (i + 1)] = l;
                    sum += l;
                }
            }
            float gx = (v[2] + 2 * v[5] + v[8]) - (v[0] + 2 * v[3] + v[6]);
            float gy = (v[6] + 2 * v[7] + v[8]) - (v[0] + 2 * v[1] + v[2]);
            float norm = sqrt(gx * gx + gy * gy) / (sum / 9.0 + 0.05);
            if (norm > u.peakThreshold) { rgb = u.m * float3(0.2, 1.0, 0.3); }
        }
        if ((u.flags & 1u) != 0u) {
            // Judged in the surface's primaries (a working-gamut frame while proofing).
            float3 t = u.m * c.rgb;
            float hi = max(t.r, max(t.g, t.b));
            float lo = min(t.r, min(t.g, t.b));
            if (hi >= u.clipHigh) { rgb = float3(1.0, 0.05, 0.05); }
            else if (lo <= 0.0005) { rgb = float3(0.05, 0.25, 1.0); }
        }
        if (u.maskCount > 0u || u.segmentCount > 0u) {
            float2 o = toCrop(t, u);
            // AI masks: the union of the selected layer's Vision bitmaps, tinted.
            float a = 0.0;
            if (u.maskCount > 0u) { a = max(a, mask0.sample(smooth, o).r); }
            if (u.maskCount > 1u) { a = max(a, mask1.sample(smooth, o).r); }
            if (u.maskCount > 2u) { a = max(a, mask2.sample(smooth, o).r); }
            if (u.maskCount > 3u) { a = max(a, mask3.sample(smooth, o).r); }
            rgb = mix(rgb, u.maskTint.rgb, a * u.maskTint.a);
            // Brush strokes: distance to round-capped segments, in long-edge units.
            float2 k = float2(u.orientedAspect, 1.0) / max(u.orientedAspect, 1.0);
            float2 q = o * k;
            float paint = 0.0, erase = 0.0;
            for (uint i = 0; i < u.segmentCount; i++) {
                float2 pa = q - segs[i].ab.xy * k, ba = (segs[i].ab.zw - segs[i].ab.xy) * k;
                float h = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-12), 0.0, 1.0);
                float d = length(pa - ba * h);
                float cover = 1.0 - smoothstep(segs[i].radius * 0.9, segs[i].radius, d);
                if (segs[i].kind == 0u) { paint = max(paint, cover); } else { erase = max(erase, cover); }
            }
            rgb = mix(rgb, float3(1.0, 0.1, 0.1), paint * 0.45);
            rgb = mix(rgb, float3(0.1, 0.3, 1.0), erase * 0.45);
        }
        return float4(rgb, 1);
    }
    """

    public init(device: MTLDevice? = MTLCreateSystemDefaultDevice(), pixelFormat: MTLPixelFormat = .bgra8Unorm_srgb) throws {
        guard let device else { throw SetupError.noDevice }
        guard let queue = device.makeCommandQueue() else { throw SetupError.noQueue }
        self.device = device
        self.queue = queue
        self.pixelFormat = pixelFormat
        do {
            library = try device.makeLibrary(source: Self.shaderSource, options: nil)
        } catch {
            throw SetupError.shader(String(describing: error))
        }
        let sd = MTLSamplerDescriptor()
        sd.minFilter = .linear
        sd.magFilter = .linear
        sd.mipFilter = .linear // trilinear for minified (compare) frames
        sd.sAddressMode = .clampToEdge
        sd.tAddressMode = .clampToEdge
        smoothSampler = device.makeSamplerState(descriptor: sd)!
        let nd = MTLSamplerDescriptor()
        nd.minFilter = .linear
        nd.magFilter = .nearest // crisp pixels at 100%+
        nd.mipFilter = .notMipmapped
        nd.sAddressMode = .clampToEdge
        nd.tAddressMode = .clampToEdge
        nearestSampler = device.makeSamplerState(descriptor: nd)!
        noProofLUT = Self.makeProofTexture(device: device, size: 2, rgba: [Float](repeating: 0, count: 32))!
        queue.label = "viewer"
        _ = try pipeline(for: pixelFormat)
    }

    /// Pipeline for a target format (cached; SDR and EDR targets differ).
    func pipeline(for format: MTLPixelFormat) throws -> MTLRenderPipelineState {
        try lock.withLock {
            if let p = pipelines[format] { return p }
            let desc = MTLRenderPipelineDescriptor()
            desc.vertexFunction = library.makeFunction(name: "viewer_vertex")
            desc.fragmentFunction = library.makeFunction(name: "viewer_fragment")
            desc.colorAttachments[0].pixelFormat = format
            do {
                let p = try device.makeRenderPipelineState(descriptor: desc)
                pipelines[format] = p
                return p
            } catch {
                throw SetupError.shader(String(describing: error))
            }
        }
    }

    /// Texture pixel format for a frame format. 8-bit frames are sRGB-encoded
    /// (sampling decodes to linear); 16F frames are already linear P3.
    public static func textureFormat(for f: FramePixelFormat) -> MTLPixelFormat {
        f.isEightBit ? .rgba8Unorm_srgb : .rgba16Float
    }

    /// Zero-copy texture over a Rust `MTLBuffer` frame (no mips), or nil when the
    /// frame is plain memory or its row stride isn't aligned for a linear texture.
    public func zeroCopyTexture(for frame: RenderedFrame) -> MTLTexture? {
        guard case .buffer(let buf) = frame.backing,
              MetalLimits.canMakeTexture(device, width: frame.width, height: frame.height) else { return nil }
        let format = Self.textureFormat(for: frame.format)
        guard frame.bytesPerRow % device.minimumLinearTextureAlignment(for: format) == 0 else { return nil }
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: frame.width, height: frame.height,
                                                            mipmapped: false)
        desc.usage = .shaderRead
        desc.storageMode = buf.storageMode
        return buf.makeTexture(descriptor: desc, offset: 0, bytesPerRow: frame.bytesPerRow)
    }

    /// Upload a frame into a new texture, with a full mip chain when `mipmapped`.
    /// Without mips, a Metal-buffer frame becomes a zero-copy texture over the
    /// buffer Rust wrote into. Thread-safe; runs on the uploader's queue.
    public func makeTexture(from frame: RenderedFrame, mipmapped: Bool = false) -> MTLTexture? {
        if !mipmapped, let tex = zeroCopyTexture(for: frame) { return tex }
        guard MetalLimits.canMakeTexture(device, width: frame.width, height: frame.height) else { return nil }
        let format = Self.textureFormat(for: frame.format)
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: frame.width, height: frame.height,
                                                            mipmapped: mipmapped)
        desc.usage = .shaderRead
        desc.storageMode = .shared
        guard let tex = device.makeTexture(descriptor: desc) else { return nil }
        upload(frame, into: tex)
        return tex
    }

    /// Copy a frame's pixels into an existing texture of the same size and format
    /// (texture reuse), then rebuild its mip chain. Buffer-backed frames are copied
    /// on the GPU (blit), plain memory with `replace(region:)`.
    public func upload(_ frame: RenderedFrame, into tex: MTLTexture) {
        let needsBlit: Bool
        if case .buffer = frame.backing { needsBlit = true } else { needsBlit = false }
        if !needsBlit {
            frame.withBytes { raw in
                tex.replace(region: MTLRegionMake2D(0, 0, frame.width, frame.height), mipmapLevel: 0,
                            withBytes: raw.baseAddress!, bytesPerRow: frame.bytesPerRow)
            }
        }
        guard needsBlit || tex.mipmapLevelCount > 1, let cb = queue.makeCommandBuffer(),
              let blit = cb.makeBlitCommandEncoder() else { return }
        if case .buffer(let buf) = frame.backing {
            blit.copy(from: buf, sourceOffset: 0, sourceBytesPerRow: frame.bytesPerRow,
                      sourceBytesPerImage: frame.bytesPerRow * frame.height,
                      sourceSize: MTLSize(width: frame.width, height: frame.height, depth: 1),
                      to: tex, destinationSlice: 0, destinationLevel: 0, destinationOrigin: MTLOrigin())
        }
        if tex.mipmapLevelCount > 1 { blit.generateMipmaps(for: tex) }
        blit.endEncoding()
        cb.commit()
        cb.waitUntilCompleted()
    }

    public func makeCommandBuffer() -> MTLCommandBuffer? { queue.makeCommandBuffer() }

    /// A soft-proof table as a 3D texture (`size`³ RGBA, red fastest; half floats,
    /// hardware-filtered), or nil when `rgba` doesn't hold `size`³ texels.
    public static func makeProofTexture(device: MTLDevice, size: Int, rgba: [Float]) -> MTLTexture? {
        guard size >= 2, rgba.count == size * size * size * 4 else { return nil }
        let desc = MTLTextureDescriptor()
        desc.textureType = .type3D
        desc.pixelFormat = .rgba16Float
        desc.width = size
        desc.height = size
        desc.depth = size
        desc.usage = .shaderRead
        desc.storageMode = .shared
        guard let tex = device.makeTexture(descriptor: desc) else { return nil }
        let halves = rgba.map { Float16($0) }
        halves.withUnsafeBytes {
            tex.replace(region: MTLRegionMake3D(0, 0, 0, size, size, size), mipmapLevel: 0, slice: 0,
                        withBytes: $0.baseAddress!, bytesPerRow: size * 8, bytesPerImage: size * size * 8)
        }
        return tex
    }

    /// Encode all panes into `pass`. `viewPixelWidth` sizes the split line.
    public func encode(_ panes: [Pane], pass: MTLRenderPassDescriptor, commandBuffer: MTLCommandBuffer,
                       viewPixelWidth: Int, targetFormat: MTLPixelFormat? = nil) {
        guard let pipeline = try? pipeline(for: targetFormat ?? pixelFormat),
              let enc = commandBuffer.makeRenderCommandEncoder(descriptor: pass) else { return }
        enc.setRenderPipelineState(pipeline)
        enc.setFragmentSamplerState(smoothSampler, index: 0)
        enc.setFragmentSamplerState(nearestSampler, index: 1)
        for pane in panes {
            var flags: UInt32 = 0
            if pane.clippingWarning { flags |= Flag.clipping }
            if pane.focusPeaking { flags |= Flag.peaking }
            if pane.circle { flags |= Flag.circle }
            if pane.nearest { flags |= Flag.nearest }
            if pane.displaySpace { flags |= Flag.displaySpace }
            if pane.cropAffine != nil { flags |= Flag.cropAffine }
            if pane.proofLUT != nil { flags |= Flag.proof }
            if pane.proofLUT != nil, pane.gamutWarning { flags |= Flag.gamutWarning }
            let isFloat = pane.after.pixelFormat == .rgba16Float
            var u = Uniforms(m: pane.colorMatrix, rect: pane.rect, clip: pane.clip, span: pane.span, region: pane.region,
                             split: pane.before == nil ? -2 : pane.split,
                             lineHalfWidth: 1.0 / Float(max(viewPixelWidth, 1)), peakThreshold: Self.peakThreshold,
                             flags: flags, peakStep: pane.peakStep ?? SIMD2(1 / Float(max(pane.after.width, 1)), 1 / Float(max(pane.after.height, 1))), clipHigh: Self.clipHigh(isFloat: isFloat),
                             angle: pane.angle, aspect: pane.aspect, turns: pane.quarterTurns)
            let masks = Array(pane.masks.prefix(4))
            u.maskCount = UInt32(masks.count)
            u.maskTint = SIMD4(pane.colorMatrix * SIMD3(pane.maskTint.x, pane.maskTint.y, pane.maskTint.z), pane.maskTint.w)
            var segs = OverlaySegment.decimate(pane.segments, to: Self.maxSegments)
            u.segmentCount = UInt32(segs.count)
            u.orientedAspect = pane.cropAspect ?? (pane.quarterTurns % 2 == 1 ? 1 / max(pane.aspect, 1e-6) : pane.aspect)
            if let a = pane.cropAffine {
                u.aff0 = SIMD4(a.0, 0)
                u.aff1 = SIMD4(a.1, 0)
            }
            u.proofIn = pane.proofIn
            u.proofOut = pane.proofOut
            u.gamutColor = SIMD4(pane.gamutColor, pane.neutralTolerance)
            enc.setVertexBytes(&u, length: MemoryLayout<Uniforms>.stride, index: 0)
            enc.setFragmentBytes(&u, length: MemoryLayout<Uniforms>.stride, index: 0)
            enc.setFragmentTexture(pane.after, index: 0)
            enc.setFragmentTexture(pane.before ?? pane.after, index: 1)
            for i in 0..<4 { enc.setFragmentTexture(i < masks.count ? masks[i] : pane.after, index: 2 + i) }
            enc.setFragmentTexture(pane.proofLUT ?? noProofLUT, index: 6)
            if segs.isEmpty { segs = [OverlaySegment(a: .zero, b: .zero, radius: 0, kind: 0)] }
            segs.withUnsafeBytes { enc.setFragmentBytes($0.baseAddress!, length: $0.count, index: 1) }
            enc.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        }
        enc.endEncoding()
    }

    private func offscreenTarget(_ format: MTLPixelFormat, _ width: Int, _ height: Int, _ panes: [Pane],
                                 clear: MTLClearColor) -> MTLTexture? {
        guard MetalLimits.canMakeTexture(device, width: width, height: height) else { return nil }
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: width, height: height, mipmapped: false)
        desc.usage = [.renderTarget, .shaderRead]
        desc.storageMode = .shared
        guard let target = device.makeTexture(descriptor: desc), let cb = makeCommandBuffer() else { return nil }
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = target
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].storeAction = .store
        pass.colorAttachments[0].clearColor = clear
        encode(panes, pass: pass, commandBuffer: cb, viewPixelWidth: width, targetFormat: format)
        cb.commit()
        cb.waitUntilCompleted()
        return target
    }

    /// Render panes into an 8-bit offscreen texture and read it back as RGBA (used by `--self-test`).
    public func renderOffscreen(_ panes: [Pane], width: Int, height: Int, clear: MTLClearColor) -> [UInt8]? {
        guard let target = offscreenTarget(pixelFormat, width, height, panes, clear: clear) else { return nil }
        var bytes = [UInt8](repeating: 0, count: width * height * 4)
        target.getBytes(&bytes, bytesPerRow: width * 4, from: MTLRegionMake2D(0, 0, width, height), mipmapLevel: 0)
        if pixelFormat == .bgra8Unorm_srgb || pixelFormat == .bgra8Unorm {
            for i in stride(from: 0, to: bytes.count, by: 4) { bytes.swapAt(i, i + 2) }
        }
        return bytes
    }

    /// Render panes into an `rgba16Float` (EDR) target and read back linear floats.
    public func renderOffscreenFloat(_ panes: [Pane], width: Int, height: Int) -> [Float]? {
        guard let target = offscreenTarget(.rgba16Float, width, height, panes,
                                           clear: MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)) else { return nil }
        var halves = [Float16](repeating: 0, count: width * height * 4)
        target.getBytes(&halves, bytesPerRow: width * 8, from: MTLRegionMake2D(0, 0, width, height), mipmapLevel: 0)
        return halves.map { Float($0) }
    }
}

/// One brush-stroke segment for the viewer shader (matches `Segment` in the shader, 32 bytes).
/// Endpoints in oriented normalized coordinates; `radius` relative to the long edge.
public struct OverlaySegment: Equatable, Sendable {
    public var ab: SIMD4<Float>
    public var radius: Float
    /// 0 = paint (red), 1 = erase (blue).
    public var kind: UInt32
    var pad = SIMD2<Float>(0, 0)

    public init(a: SIMD2<Float>, b: SIMD2<Float>, radius: Float, kind: UInt32) {
        ab = SIMD4(a.x, a.y, b.x, b.y)
        self.radius = radius
        self.kind = kind
    }

    /// Segments along a polyline (a single point becomes a dot).
    public static func segments(_ pts: [CGPoint], radius: Float, erase: Bool) -> [OverlaySegment] {
        let kind: UInt32 = erase ? 1 : 0
        let v = pts.map { SIMD2(Float($0.x), Float($0.y)) }
        guard let first = v.first else { return [] }
        if v.count == 1 { return [OverlaySegment(a: first, b: first, radius: radius, kind: kind)] }
        return zip(v, v.dropFirst()).map { OverlaySegment(a: $0, b: $1, radius: radius, kind: kind) }
    }

    /// Keep at most `limit` segments by merging consecutive ones of the same stroke (evenly).
    public static func decimate(_ segs: [OverlaySegment], to limit: Int) -> [OverlaySegment] {
        guard segs.count > limit, limit > 0 else { return segs }
        let step = Double(segs.count) / Double(limit)
        return (0..<limit).map { i in
            let lo = Int(Double(i) * step), hi = min(segs.count - 1, Int(Double(i + 1) * step) - 1)
            var s = segs[lo]
            s.ab = SIMD4(segs[lo].ab.x, segs[lo].ab.y, segs[max(lo, hi)].ab.z, segs[max(lo, hi)].ab.w)
            return s
        }
    }
}

/// Color-space matrices between linear sRGB and linear Display P3 (D65).
public enum ViewerColor {
    public static let srgbToP3 = simd_float3x3(rows: [
        SIMD3(0.822462, 0.177538, 0.0),
        SIMD3(0.033194, 0.966806, 0.0),
        SIMD3(0.017083, 0.072397, 0.910520),
    ])
    public static let p3ToSrgb = srgbToP3.inverse
    public static let rec2020ToP3 = simd_float3x3(rows: [
        SIMD3(1.343578, -0.28218, -0.061399),
        SIMD3(-0.065297, 1.075788, -0.01049),
        SIMD3(0.002822, -0.019598, 1.016777),
    ])
    public static let rec2020ToSrgb = simd_float3x3(rows: [
        SIMD3(1.660491, -0.587641, -0.07285),
        SIMD3(-0.12455, 1.1329, -0.008349),
        SIMD3(-0.018151, -0.100579, 1.11873),
    ])

    public static func matrix(source: FramePixelFormat, edrTarget: Bool) -> simd_float3x3 {
        matrix(source: source, p3Target: edrTarget)
    }

    /// Matrix taking a frame's linear color into the target surface's primaries
    /// (Display P3 for EDR and wide-gamut SDR surfaces, else sRGB).
    public static func matrix(source: FramePixelFormat, p3Target: Bool) -> simd_float3x3 {
        if source == .rgba16fLinearRec2020 { return p3Target ? rec2020ToP3 : rec2020ToSrgb }
        if source.isP3 == p3Target { return matrix_identity_float3x3 }
        return source.isP3 ? p3ToSrgb : srgbToP3
    }

    /// A frame's linear colour → linear Rec.2020 (the soft-proof table's input).
    public static func toRec2020(_ source: FramePixelFormat) -> simd_float3x3 {
        if source == .rgba16fLinearRec2020 { return matrix_identity_float3x3 }
        return (source.isP3 ? rec2020ToP3 : rec2020ToSrgb).inverse
    }

    /// Linear Rec.2020 (the soft-proof table's output) → the target surface's primaries.
    public static func fromRec2020(p3Target: Bool) -> simd_float3x3 { p3Target ? rec2020ToP3 : rec2020ToSrgb }
}
