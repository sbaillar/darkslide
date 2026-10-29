import CoreGraphics
import LuxtableFFI
import Foundation
import Metal

/// The FFI contract revision these Swift sources were written against.
/// `SelfTest` fails if the linked Rust library reports a different one.
public let expectedFFIContract: UInt32 = 47

/// `Renderer` backed by the Rust `Engine`.
///
/// Until `lt-pipeline` is exposed it renders the engine's test pattern over a
/// virtual image of the photo's pixel size, with the edit's exposure, white
/// balance, and saturation applied in Rust. Requests are honoured exactly
/// (fit size, scale, region/tile, 8-bit or 16F linear P3), so the viewer's
/// tiled 100% path is exercised for real.
///
/// Memory: Rust writes straight into a shared-storage `MTLBuffer` this type
/// allocates (0 CPU copies; the viewer makes its texture from the buffer).
/// Without a Metal device it falls back to heap memory wrapped as `Data`.
/// Cancellation: the Swift task's cancellation sets a `CancelToken` that the
/// Rust worker checks every 64 rows.
public final class RustRenderer: Renderer {
    /// The engine, or why it couldn't be created (surfaced as a render error, never a trap: C3/L12).
    private let engineResult: Result<Engine, Error>
    private let device: MTLDevice?

    public init(engine: Engine? = nil, device: MTLDevice? = MTLCreateSystemDefaultDevice()) {
        engineResult = engine.map { .success($0) } ?? Result { try Engine() }
        self.device = device
    }

    public func render(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> RenderedFrame {
        let (sw, sh) = Self.sourceSize(photo)
        let ffiRequest = Self.ffiRequest(request)
        let info: FrameInfo
        do {
            info = try planFrame(sourceWidth: sw, sourceHeight: sh, request: ffiRequest)
        } catch let e as DsError {
            throw RenderError(e)
        }
        let dest = try FrameDestination(length: Int(info.bytesPerRow) * Int(info.height), device: device)
        let token = try CancelToken()
        let engine = try engineResult.get()
        do {
            try Task.checkCancellation()
            let done = try await withTaskCancellationHandler {
                try await engine.renderTestPatternInto(
                    sourceWidth: sw, sourceHeight: sh, params: Self.params(for: edit), request: ffiRequest,
                    destPtr: UInt64(UInt(bitPattern: dest.pointer)), destLen: UInt64(dest.length), token: token)
            } onCancel: {
                try? token.cancel()
            }
            return dest.frame(Self.frameGeometry(done))
        } catch let e as DsError {
            dest.discard()
            if case .Cancelled = e { throw CancellationError() }
            throw RenderError(e)
        } catch {
            dest.discard()
            throw error
        }
    }

    /// Oriented pixel size of the photo (the pattern's virtual full image).
    static func sourceSize(_ photo: Photo) -> (UInt32, UInt32) {
        let w = max(1, photo.meta.pixelWidth), h = max(1, photo.meta.pixelHeight)
        return (UInt32(min(w, 65_536)), UInt32(min(h, 65_536)))
    }

    static func ffiRequest(_ r: RenderRequest) -> LuxtableFFI.RenderRequest {
        LuxtableFFI.RenderRequest(
            maxLongEdge: UInt32(clamping: max(1, r.maxPixelSize)),
            scale: r.scale.map { Float($0) },
            region: r.region.map { NormRect(x: Float($0.minX), y: Float($0.minY), w: Float($0.width), h: Float($0.height)) },
            format: r.format.isEightBit ? .rgba8Srgb : .rgba16fLinearP3)
    }

    struct Geometry: Equatable {
        var width: Int, height: Int, bytesPerRow: Int
        var format: FramePixelFormat
        var region: CGRect?
        var fullSize: CGSize
    }

    static func frameGeometry(_ i: FrameInfo) -> Geometry {
        Geometry(
            width: Int(i.width), height: Int(i.height), bytesPerRow: Int(i.bytesPerRow),
            format: i.format == .rgba8Srgb ? .rgba8Srgb : .rgba16fLinearP3,
            region: i.region.map { CGRect(x: CGFloat($0.x), y: CGFloat($0.y), width: CGFloat($0.w), height: CGFloat($0.h)) },
            fullSize: CGSize(width: Int(i.fullWidth), height: Int(i.fullHeight)))
    }

    /// Map the UI edit onto the test pattern's adjustable parameters.
    static func params(for edit: EditState) -> TestPatternParams {
        let warmth = edit.whiteBalance.asShot ? 0 : ((edit.whiteBalance.temperature - 5500) / 4500).clamped(-1, 1)
        return TestPatternParams(
            exposureEv: Float(edit.exposure.ev.clamped(-5, 5)),
            warmth: Float(warmth),
            saturation: Float((edit.exposure.saturation / 100).clamped(-1, 1)))
    }
}

/// Memory Rust renders into: a shared `MTLBuffer` (normal case) or heap bytes.
/// Kept alive by the caller across the `await`, so the pointer stays valid for
/// the whole Rust call (the FFI contract).
final class FrameDestination: @unchecked Sendable {
    let pointer: UnsafeMutableRawPointer
    let length: Int
    private let buffer: MTLBuffer?
    private var owned = true

    /// `pool`: back the Metal buffer with recycled page-aligned memory (M2: no fresh
    /// 21/43 MB allocation per 5K frame; the block returns to the pool when the frame's
    /// buffer, and every texture made from it, is released).
    init(length: Int, device: MTLDevice?, pool: FrameMemoryPool? = nil) throws {
        self.length = max(length, 1)
        if let device, let pool, let pooled = pool.buffer(length: self.length, device: device) {
            buffer = pooled.buffer
            pointer = pooled.pointer
        } else if let device, MetalLimits.canMakeBuffer(device, length: self.length),
                  let b = device.makeBuffer(length: self.length, options: .storageModeShared) {
            buffer = b
            pointer = b.contents()
        } else {
            buffer = nil
            pointer = .allocate(byteCount: self.length, alignment: 256)
        }
    }

    /// Hand the pixels to a frame (ownership moves to the frame).
    func frame(_ g: RustRenderer.Geometry, mapping: FrameMapping? = nil, histogram: Histogram? = nil) -> RenderedFrame {
        owned = false
        let backing: FrameBacking = if let buffer {
            .buffer(buffer)
        } else {
            .data(Data(bytesNoCopy: pointer, count: length, deallocator: .custom { p, _ in p.deallocate() }))
        }
        return RenderedFrame(width: g.width, height: g.height, format: g.format, bytesPerRow: g.bytesPerRow,
                             backing: backing, region: g.region, fullSize: g.fullSize, mapping: mapping, histogram: histogram)
    }

    /// Free heap memory after a failed render (Metal buffers free themselves).
    func discard() {
        guard owned else { return }
        owned = false
        if buffer == nil { pointer.deallocate() }
    }

    deinit { discard() }

    var isMetal: Bool { buffer != nil }
}

/// A render failure with a message fit for the UI.
public struct RenderError: Error, Equatable, LocalizedError {
    public enum Kind: Sendable, Equatable { case invalidArgument, cancelled, io, unsupported, internalError }
    public let kind: Kind
    public let message: String

    init(_ e: DsError) {
        switch e {
        case .InvalidArgument(let m): kind = .invalidArgument; message = m
        case .Cancelled: kind = .cancelled; message = "Cancelled"
        case .Io(let m): kind = .io; message = m
        case .Unsupported(let m): kind = .unsupported; message = m
        case .Internal(let m): kind = .internalError; message = "Internal error: \(m)"
        case .FolderMoved(let m): kind = .io; message = "The folder was moved: \(m)"
        }
    }

    public var errorDescription: String? { message }
}

/// Rust-side metadata the UI checks against its own tables.
public enum RustCore {
    public static var version: String { (try? LuxtableFFI.version()) ?? "unknown" }
    public static var contract: UInt32 { (try? ffiContract()) ?? 0 }
    /// `(group, supportsRelative)` in Rust order, mapped to the Swift enum.
    public static var groups: [(AdjustGroup, Bool)] {
        ((try? adjustGroups()) ?? []).map { (AdjustGroup(rust: $0.group), $0.supportsRelative) }
    }
}

extension AdjustGroup {
    var rust: LuxtableFFI.AdjustGroup {
        switch self {
        case .profile: .profile
        case .whiteBalance: .whiteBalance
        case .exposure: .exposure
        case .presence: .presence
        case .hdr: .hdr
        case .levels: .levels
        case .curve: .curve
        case .colorBalance: .colorBalance
        case .colorEditor: .colorEditor
        case .detail: .detail
        case .noise: .noise
        case .vignette: .vignette
        case .effects: .effects
        case .lens: .lens
        case .crop: .crop
        case .layers: .layers
        case .spots: .spots
        }
    }

    init(rust: LuxtableFFI.AdjustGroup) {
        switch rust {
        case .profile: self = .profile
        case .whiteBalance: self = .whiteBalance
        case .exposure: self = .exposure
        case .presence: self = .presence
        case .hdr: self = .hdr
        case .levels: self = .levels
        case .curve: self = .curve
        case .colorBalance: self = .colorBalance
        case .colorEditor: self = .colorEditor
        case .detail: self = .detail
        case .noise: self = .noise
        case .vignette: self = .vignette
        case .effects: self = .effects
        case .lens: self = .lens
        case .crop: self = .crop
        case .layers: self = .layers
        case .spots: self = .spots
        }
    }
}
