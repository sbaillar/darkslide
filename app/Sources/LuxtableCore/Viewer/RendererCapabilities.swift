import Foundation

/// What a renderer can do beyond a whole-image 8-bit render (N1, H6).
public struct RendererCapabilities: Equatable, Sendable {
    /// Honours `RenderRequest.region`/`scale` (real 1:1 tiles).
    public var regions: Bool
    /// Can emit `.rgba16fLinearP3` frames (EDR).
    public var floatFrames: Bool
    /// Renders the edit (false: shows the unedited embedded preview).
    public var rendersEdits: Bool
    /// Adjustment groups actually rendered when `rendersEdits` (nil = all).
    public var renderedGroups: Set<AdjustGroup>?
    /// Renders the final geometry (crop, straighten, turns, lens, keystone) for
    /// `RenderGeometry.final` requests and returns frames with a `FrameMapping`.
    public var rendersGeometry: Bool
    /// Develops on the CPU because no GPU is available (the viewer says so).
    public var cpuFallback: Bool

    public init(regions: Bool, floatFrames: Bool, rendersEdits: Bool, renderedGroups: Set<AdjustGroup>? = nil,
                rendersGeometry: Bool = false, cpuFallback: Bool = false) {
        self.regions = regions
        self.floatFrames = floatFrames
        self.rendersEdits = rendersEdits
        self.renderedGroups = renderedGroups
        self.rendersGeometry = rendersGeometry
        self.cpuFallback = cpuFallback
    }

    /// A renderer that promises nothing beyond whole 8-bit frames.
    public static let basic = RendererCapabilities(regions: false, floatFrames: false, rendersEdits: true)

    /// Capabilities of any renderer (renderers opt in via `CapableRenderer`).
    public static func of(_ r: any Renderer) -> RendererCapabilities {
        (r as? any CapableRenderer)?.capabilities ?? .basic
    }
}

/// Renderers that can warm a photo's develop (decode, GPU state) in the background and
/// report its final-frame mapping (the viewer uses it to place the preview).
public protocol GeometryPrefetching: Renderer {
    func prefetch(_ photo: Photo, edit: EditState, request: RenderRequest) async -> FrameMapping?
}

/// Renderers whose background prefetches can be cancelled when the user navigates away.
public protocol PrefetchCancelling: Renderer {
    func cancelPrefetches(keeping: [Photo])
}

/// Renderers declare their capabilities by conforming.
public protocol CapableRenderer: Renderer {
    var capabilities: RendererCapabilities { get }
}

extension RustRenderer: CapableRenderer {
    /// The Rust engine renders regions at any scale and emits 16F linear P3.
    /// Until lt-pipeline lands it renders the test pattern, which shows only
    /// exposure (EV, saturation) and white balance (warmth).
    public var capabilities: RendererCapabilities {
        RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true,
                             renderedGroups: [.exposure, .whiteBalance])
    }
}

extension EmbeddedPreviewRenderer: CapableRenderer {
    /// Whole embedded JPEG only; edits are not applied.
    public var capabilities: RendererCapabilities {
        RendererCapabilities(regions: false, floatFrames: false, rendersEdits: false)
    }
}
