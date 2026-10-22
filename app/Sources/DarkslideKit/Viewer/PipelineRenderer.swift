import CoreGraphics
import DarkslideFFI
import Foundation
import Metal

/// Geometry of a frame a pipeline backend will write (mirrors ds-ffi `FrameInfo`).
public struct PipelineFramePlan: Sendable, Equatable {
    public var width: Int
    public var height: Int
    public var bytesPerRow: Int
    public var format: FramePixelFormat
    public var region: CGRect?
    public var fullSize: CGSize

    public init(width: Int, height: Int, bytesPerRow: Int, format: FramePixelFormat, region: CGRect?, fullSize: CGSize) {
        self.width = width
        self.height = height
        self.bytesPerRow = bytesPerRow
        self.format = format
        self.region = region
        self.fullSize = fullSize
    }
}

/// The seam ds-pipeline plugs into (H6/M3): plan a frame, then render it straight
/// into caller memory, as ds-pipeline's `render_into(format, &mut [u8])` will.
/// Implementations must write `plan.bytesPerRow * plan.height` bytes, stop and
/// throw `CancellationError` promptly when the task is cancelled, and never touch
/// `dest` after returning.
public protocol PipelineBackend: Sendable {
    var capabilities: RendererCapabilities { get }
    func plan(_ photo: Photo, edit: EditState, request: RenderRequest) throws -> PipelineFramePlan
    func render(_ photo: Photo, edit: EditState, request: RenderRequest, plan: PipelineFramePlan,
                into dest: UnsafeMutableRawPointer, length: Int) async throws
}

/// `Renderer` over a `PipelineBackend`: allocates one shared-storage `MTLBuffer`
/// per frame, lets the backend render into it, and hands the buffer to the viewer,
/// which wraps it in a texture without copying (fit renders are requested at
/// display size, so no mip chain and no full-frame blit are needed). CPU copies
/// per displayed frame: 0. It asks for `.rgba16fLinearP3` when the viewer does
/// (EDR displays), per the request.
public final class PipelineRenderer: CapableRenderer {
    public let backend: any PipelineBackend
    private let device: MTLDevice?

    public init(backend: any PipelineBackend, device: MTLDevice? = MTLCreateSystemDefaultDevice()) {
        self.backend = backend
        self.device = device
    }

    public var capabilities: RendererCapabilities { backend.capabilities }

    public func render(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> RenderedFrame {
        try Task.checkCancellation()
        let plan = try backend.plan(photo, edit: edit, request: request)
        let dest = try FrameDestination(length: plan.bytesPerRow * plan.height, device: device)
        do {
            try await backend.render(photo, edit: edit, request: request, plan: plan, into: dest.pointer, length: dest.length)
            try Task.checkCancellation()
        } catch {
            dest.discard()
            throw error
        }
        return dest.frame(RustRenderer.Geometry(width: plan.width, height: plan.height, bytesPerRow: plan.bytesPerRow,
                                                format: plan.format, region: plan.region, fullSize: plan.fullSize))
    }
}

/// Stand-in backend until ds-pipeline is merged: the ds-ffi engine's test pattern,
/// rendered into caller memory through the same `render_*_into` contract.
public struct TestPatternPipelineBackend: PipelineBackend {
    let engineResult: Result<Engine, Error>

    public init(engine: Engine? = nil) { engineResult = engine.map { .success($0) } ?? Result { try Engine() } }

    public var capabilities: RendererCapabilities {
        RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true, renderedGroups: [.exposure, .whiteBalance])
    }

    public func plan(_ photo: Photo, edit: EditState, request: RenderRequest) throws -> PipelineFramePlan {
        let (sw, sh) = RustRenderer.sourceSize(photo)
        do {
            let g = RustRenderer.frameGeometry(try planFrame(sourceWidth: sw, sourceHeight: sh,
                                                             request: RustRenderer.ffiRequest(request)))
            return PipelineFramePlan(width: g.width, height: g.height, bytesPerRow: g.bytesPerRow, format: g.format,
                                     region: g.region, fullSize: g.fullSize)
        } catch let e as DsError {
            throw RenderError(e)
        }
    }

    public func render(_ photo: Photo, edit: EditState, request: RenderRequest, plan: PipelineFramePlan,
                       into dest: UnsafeMutableRawPointer, length: Int) async throws {
        let (sw, sh) = RustRenderer.sourceSize(photo)
        let token = try CancelToken()
        let engine = try engineResult.get()
        let address = UInt64(UInt(bitPattern: dest))
        do {
            _ = try await withTaskCancellationHandler {
                try await engine.renderTestPatternInto(
                    sourceWidth: sw, sourceHeight: sh, params: RustRenderer.params(for: edit),
                    request: RustRenderer.ffiRequest(request), destPtr: address, destLen: UInt64(length), token: token)
            } onCancel: {
                try? token.cancel()
            }
        } catch let e as DsError {
            if case .Cancelled = e { throw CancellationError() }
            throw RenderError(e)
        }
    }
}
