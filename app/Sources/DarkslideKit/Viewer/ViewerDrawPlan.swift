import CoreGraphics
import simd

/// Everything the Metal surface needs for one draw, decided without Metal.
///
/// The view's coordinator only resolves each quad's frame to a texture (via
/// `TextureUploader`) and encodes; which frames are drawn where, with which
/// filtering, display transform, color matrix and overlays is decided here and
/// unit-tested.
public struct ViewerDrawPlan: Equatable {
    public struct Quad: Equatable {
        public var frame: RenderedFrame
        public var before: RenderedFrame?
        /// Destination in NDC.
        public var rect: SIMD4<Float>
        public var clip: SIMD4<Float>
        /// Display-space rect the quad covers (x0, y0, x1, y1).
        public var span: SIMD4<Float> = SIMD4(0, 0, 1, 1)
        /// Oriented-space rect the frame holds (x0, y0, x1, y1).
        public var region: SIMD4<Float> = SIMD4(0, 0, 1, 1)
        public var quarterTurns: UInt32 = 0
        /// Straighten, radians (clockwise).
        public var angle: Float = 0
        /// Displayed image width / height.
        public var aspect: Float = 1
        /// Peaking step: one device pixel in display-normalized units.
        public var peakStep = SIMD2<Float>(0.001, 0.001)
        public var split: Float = -2
        public var nearest = false
        /// The frame is shown noticeably smaller than its pixel size: needs a mip chain.
        public var mipmapped = false
        public var circle = false
        public var clippingWarning = false
        public var focusPeaking = false
        public var colorMatrix = matrix_identity_float3x3
        /// Canvas overlay drawn in the shader: Vision masks (by cache key) and brush strokes.
        public var masks: [MaskOverlayCache.Key] = []
        public var segments: [OverlaySegment] = []
        /// The frame is a developed final frame: display space itself (see `MetalViewerRenderer.Pane`).
        public var displaySpace = false
        /// Display -> crop space (developed display): oriented frames, masks and strokes go through it.
        public var cropAffine: [Float]?
        public var cropAspect: Float?
        /// Soft proof (nil: off), with the frame's linear colour → linear Rec.2020
        /// (`proofIn`) and linear Rec.2020 → the surface's primaries (`proofOut`).
        public var proof: ProofDisplay?
        public var proofIn = matrix_identity_float3x3
        public var proofOut = matrix_identity_float3x3
        /// The gamut warning colour in the surface's primaries (linear).
        public var gamutColor = SIMD3<Float>(0.5, 0.5, 0.5)
        /// Gray proof profiles' chroma limit (see `MetalViewerRenderer.Pane`); 0: none.
        public var neutralTolerance: Float = 0
    }

    /// Loupe: a magnified circle centered on `center` (view points).
    public struct Loupe: Equatable {
        public var center: CGPoint
        public var radius: CGFloat
        /// Magnification relative to 100% (device pixels per image pixel).
        public var scale: Double

        public init(center: CGPoint, radius: CGFloat = 110, scale: Double = 2) {
            self.center = center
            self.radius = radius
            self.scale = scale
        }
    }

    public struct Input {
        public var layout: ViewerLayout
        public var viewSize: CGSize
        public var backingScale: CGFloat
        /// Frame shown in the main pane (develop render, else embedded preview), already
        /// validated to belong to the focused photo.
        public var main: RenderedFrame?
        /// 1:1 tile over part of the main image (`region` normalized, oriented space).
        public var tile: (frame: RenderedFrame, region: CGRect)?
        public var before: RenderedFrame?
        public var compare: RenderedFrame?
        public var clippingWarning = false
        public var focusPeaking = false
        public var loupe: Loupe?
        /// Pixel size of the main photo, oriented (for loupe magnification).
        public var imageSize: CGSize = .zero
        /// How the main image is displayed (quarter turns + straighten).
        public var transform = ViewTransform()
        /// How the pinned (compare) image is displayed.
        public var compareTransform = ViewTransform()
        /// Developed display (the pipeline renders the final geometry): display space is
        /// the final frame; oriented frames and overlays are placed through the mapping.
        public var mapping: FrameMapping?
        public var compareMapping: FrameMapping?
        /// Width/height of the main photo's crop space (oriented image), for strokes.
        public var orientedAspect: CGFloat = 1
        /// The display is wide-gamut (Display P3): SDR frames are drawn into a P3 surface.
        public var wideGamut = false
        /// The viewer renders EDR frames on this display (headroom > 1): keep the surface
        /// extended-range for every frame, 8-bit previews included, so it doesn't switch
        /// format (and blank a frame) at each navigation (L6).
        public var edrDisplay = false
        /// Selected layer's Vision masks and brush strokes (main image only).
        public var overlayMasks: [MaskOverlayCache.Key] = []
        public var overlaySegments: [OverlaySegment] = []
        /// Soft proof applied to every pane (nil: off).
        public var proof: ProofDisplay?

        public init(layout: ViewerLayout, viewSize: CGSize, backingScale: CGFloat, main: RenderedFrame?,
                    tile: (frame: RenderedFrame, region: CGRect)? = nil, before: RenderedFrame? = nil,
                    compare: RenderedFrame? = nil) {
            self.layout = layout
            self.viewSize = viewSize
            self.backingScale = backingScale
            self.main = main
            self.tile = tile
            self.before = before
            self.compare = compare
        }
    }

    public var quads: [Quad]
    /// Draw into an extended-range (rgba16Float, linear Display P3) surface.
    public var edr: Bool
    /// Draw into a Display P3 SDR surface (P3 frames on a wide-gamut display without EDR).
    public var wide = false

    /// Frame ids the plan uses (everything else may be evicted).
    public var frameIDs: Set<UInt64> {
        Set(quads.flatMap { [$0.frame.id] + ($0.before.map { [$0.id] } ?? []) })
    }

    public static func == (a: ViewerDrawPlan, b: ViewerDrawPlan) -> Bool {
        a.edr == b.edr && a.wide == b.wide && a.quads == b.quads
    }

    static func rect4(_ r: CGRect) -> SIMD4<Float> {
        SIMD4(Float(r.minX), Float(r.minY), Float(r.maxX), Float(r.maxY))
    }

    public static func make(_ input: Input) -> ViewerDrawPlan {
        let frames = [input.main, input.compare, input.tile?.frame, input.before].compactMap { $0 }
        let edr = input.edrDisplay || frames.contains { $0.format.isFloat }
        let wide = !edr && input.wideGamut && frames.contains { $0.format == .rgba8DisplayP3 }
        let p3 = edr || wide
        let view = input.viewSize
        var quads: [Quad] = []
        for pane in input.layout.panes {
            let isMain = pane.role == .main
            guard let frame = isMain ? input.main : input.compare else { continue }
            let t = isMain ? input.transform : input.compareTransform
            let m = isMain ? input.mapping : input.compareMapping
            var q = quad(frame, dest: pane.image, pane: pane, input: input, edr: p3, transform: t, mapping: m)
            if isMain {
                q.masks = input.overlayMasks
                q.segments = input.overlaySegments
            }
            if isMain, let before = input.before, let x = pane.splitX, view.width > 0 {
                q.before = before
                q.split = Float(x / view.width * 2 - 1)
            }
            quads.append(q)
            if isMain, let tile = input.tile {
                var tq = quad(tile.frame, dest: pane.image, pane: pane, input: input, edr: p3, transform: t,
                              mapping: input.mapping)
                // [cul9] L7: with before/after, the 1:1 tile stays on the "after" side.
                if q.before != nil { tq.clip.x = max(tq.clip.x, q.split) }
                tq.region = rect4(tile.region)
                tq.nearest = pixelsPerTexel(tile.frame, region: tile.region, pane: pane, input: input) >= 0.999
                tq.mipmapped = false
                tq.masks = input.overlayMasks
                tq.segments = input.overlaySegments
                quads.append(tq)
            }
        }
        if let loupe = input.loupe, let pane = input.layout.main, pane.frame.contains(loupe.center),
           pane.image.contains(loupe.center), pane.image.width > 0, pane.image.height > 0 {
            quads.append(loupeQuad(loupe, pane: pane, input: input, edr: p3))
        }
        return ViewerDrawPlan(quads: quads, edr: edr, wide: wide)
    }

    /// Device pixels per texel for `frame` covering `region` (oriented) drawn in `pane`.
    static func pixelsPerTexel(_ frame: RenderedFrame, region: CGRect, pane: ViewerLayout.Pane, input: Input) -> CGFloat {
        let imageLong = max(pane.image.width, pane.image.height) * input.backingScale
        let texels = CGFloat(max(frame.width, frame.height)) / max(max(region.width, region.height), 1e-6)
        return imageLong / max(texels, 1)
    }

    static func quad(_ frame: RenderedFrame, dest: CGRect, pane: ViewerLayout.Pane, input: Input, edr: Bool,
                     transform: ViewTransform, mapping: FrameMapping? = nil) -> Quad {
        let view = input.viewSize
        var q = Quad(frame: frame, rect: ViewerGeometry.ndc(dest, in: view), clip: ViewerGeometry.ndc(pane.frame, in: view))
        let ppt = pixelsPerTexel(frame, region: CGRect(x: 0, y: 0, width: 1, height: 1), pane: pane, input: input)
        // Magnifying (≥ 1 device pixel per texel): nearest keeps pixels crisp; minifying a lot: mips + trilinear.
        q.nearest = ppt >= 0.999
        q.mipmapped = ppt < 0.75
        q.quarterTurns = UInt32(transform.quarterTurns)
        q.angle = transform.angleRadians
        q.aspect = Float(pane.image.width / max(pane.image.height, 1e-6))
        q.peakStep = SIMD2(Float(1 / max(pane.image.width * input.backingScale, 1)),
                           Float(1 / max(pane.image.height * input.backingScale, 1)))
        q.clippingWarning = input.clippingWarning
        q.focusPeaking = input.focusPeaking
        q.colorMatrix = ViewerColor.matrix(source: frame.format, p3Target: edr)
        if let proof = input.proof {
            q.proof = proof
            q.proofIn = ViewerColor.toRec2020(frame.format)
            q.proofOut = ViewerColor.fromRec2020(p3Target: edr)
            q.gamutColor = ViewerColor.matrix(source: .rgba16fLinearP3, p3Target: edr) * proof.warningColor
            q.neutralTolerance = proof.neutralTolerance ?? 0
        }
        if let mapping {
            // Developed display: the final frame is display space; an oriented frame
            // (the embedded preview while the develop render is on its way) and the
            // overlays are placed through the frame's map to crop space.
            q.quarterTurns = 0
            q.angle = 0
            q.displaySpace = frame.mapping != nil
            q.cropAffine = mapping.affine.map { Float($0) }
            q.cropAspect = Float(input.orientedAspect)
            if let r = frame.region, frame.mapping != nil { q.region = rect4(r) }
        }
        return q
    }

    static func loupeQuad(_ loupe: Loupe, pane: ViewerLayout.Pane, input: Input, edr: Bool) -> Quad {
        let u = pane.normalized(loupe.center) // display space
        // Prefer the 1:1 tile when it covers the point (in oriented space).
        // Tiles hold oriented regions, or display regions of a developed frame.
        let tileU = input.mapping != nil ? u
            : input.transform.toOriented(u, displayAspect: pane.image.width / max(pane.image.height, 1e-6))
        var source = input.main!
        var region = CGRect(x: 0, y: 0, width: 1, height: 1)
        if let tile = input.tile, tile.region.contains(tileU), tile.region.width > 0, tile.region.height > 0 {
            source = tile.frame
            region = tile.region
        }
        // Span of the displayed image (normalized) covered by the loupe at `loupe.scale`.
        let display = input.mapping?.nativeSize ?? input.transform.displaySize(oriented: input.imageSize)
        let diameter = Double(loupe.radius * 2 * input.backingScale)
        let spanX = diameter / max(Double(display.width) * loupe.scale, 1)
        let spanY = diameter / max(Double(display.height) * loupe.scale, 1)
        let dest = CGRect(x: loupe.center.x - loupe.radius, y: loupe.center.y - loupe.radius,
                          width: loupe.radius * 2, height: loupe.radius * 2)
        var q = quad(source, dest: dest, pane: pane, input: input, edr: edr, transform: input.transform,
                     mapping: input.mapping)
        q.region = rect4(region)
        q.clip = SIMD4(-1, -1, 1, 1)
        q.span = SIMD4(Float(Double(u.x) - spanX / 2), Float(Double(u.y) - spanY / 2),
                       Float(Double(u.x) + spanX / 2), Float(Double(u.y) + spanY / 2))
        q.circle = true
        q.nearest = true
        q.mipmapped = false
        return q
    }
}
