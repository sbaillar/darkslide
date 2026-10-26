import CoreGraphics
import LuxtableAI
import LuxtableFFI
import Foundation
import Metal
import os

/// Live-mode `Renderer`: develops the RAW through lt-pipeline (FFI `render_develop`)
/// on the process-wide GPU renderer, straight into a shared `MTLBuffer` the viewer
/// wraps as a texture (0 CPU copies per displayed frame).
///
/// - `.final` geometry returns the pipeline's output (crop, straighten, turns, lens,
///   keystone) with its `FrameMapping` to crop space; `.oriented` the whole corrected
///   frame the crop/straighten/mask tools edit on.
/// - Region requests (1:1 tiles) render the region at `scale` output pixels per
///   sensor pixel, never upscaled.
/// - The frame's histogram (clipping counts included) comes back with the pixels.
/// - AI mask bitmaps are gathered before the render (`maskProvider`, Apple Vision) and
///   handed to the pipeline's `MaskSource`.
/// - When no GPU is available lt-ffi renders on the CPU; `backend` says so.
public final class DevelopRenderer: CapableRenderer, GeometryPrefetching, @unchecked Sendable {
    /// Gathers AI mask bitmaps (crop space) for a photo. Called before each render with
    /// AI shapes; missing masks render as empty.
    public typealias MaskProvider = @Sendable (PhotoID, Set<AITarget>) async -> [AITarget: VisionMask]

    private let device: MTLDevice?
    private let lock = NSLock()
    private var _maskProvider: MaskProvider?
    private var _personMaskProvider: PersonMaskProvider?
    private var _stats = Stats()
    /// Recycled frame memory: one block per frame size in flight, not a new
    /// 21/43 MB allocation per 5K frame.
    let pool = FrameMemoryPool()

    /// What rendering costs, counted honestly.
    public struct Stats: Equatable, Sendable {
        public var frames = 0
        public var bytes = 0
        public var lastBytes = 0
        /// CPU copies of frame pixels on the Rust side: the GPU renderer's readback copies
        /// each frame once from the mapped wgpu buffer into this renderer's Metal buffer
        /// (wgpu can't render into a foreign buffer). 1 per GPU frame.
        public var rustCopies = 0
        /// Copies on the Swift side (unaligned rows need a blit, heap fallback).
        public var swiftCopies = 0
        /// Fresh frame-memory blocks allocated (the rest were recycled from the pool).
        public var allocations = 0
        /// All CPU copies of frame pixels.
        public var copies: Int { rustCopies + swiftCopies }
    }

    public init(device: MTLDevice? = MTLCreateSystemDefaultDevice()) {
        self.device = device
    }

    public var maskProvider: MaskProvider? {
        get { lock.withLock { _maskProvider } }
        set { lock.withLock { _maskProvider = newValue } }
    }

    /// Per-person masks (`MaskShape.aiPerson`).
    public var personMaskProvider: PersonMaskProvider? {
        get { lock.withLock { _personMaskProvider } }
        set { lock.withLock { _personMaskProvider = newValue } }
    }

    public var stats: Stats { lock.withLock { _stats } }

    private static let warm = WarmState()

    /// Create the GPU renderer (device, every WGSL kernel compiled) off the main thread.
    /// The app calls this at launch; the first render or capabilities read would
    /// otherwise do it on the MainActor.
    @discardableResult
    public static func warmUp() -> Task<Void, Never> {
        warm.start()
    }

    /// Which renderer lt-ffi uses ("Apple M4 Pro", or "CPU rendering (…)"). Never
    /// blocks: before the warm-up finished it reports the optimistic GPU default (and
    /// starts the warm-up); afterwards the live value (a GPU lost at runtime shows up).
    public static var backend: (name: String, gpu: Bool) {
        guard warm.isReady else {
            warmUp()
            return ("GPU (starting)", true)
        }
        return (try? developBackend()).map { ($0.name, $0.gpu) } ?? ("CPU rendering (unavailable)", false)
    }

    public var capabilities: RendererCapabilities {
        RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true, renderedGroups: nil,
                             rendersGeometry: true, cpuFallback: !Self.backend.gpu)
    }

    // MARK: Requests

    static func ffiFormat(_ f: FramePixelFormat) -> DevelopFormat {
        switch f {
        case .rgba8Srgb: .rgba8Srgb
        case .rgba8DisplayP3: .rgba8DisplayP3
        case .rgba16fLinearP3: .rgba16fLinearP3
        case .rgba16fLinearRec2020: .rgba16fLinearRec2020
        }
    }

    static func ffiQuality(_ q: RenderQuality) -> DevelopQuality {
        switch q {
        case .draft: .draft
        case .preview: .preview
        case .full: .full
        }
    }

    static func ffiRequest(_ r: RenderRequest, maxWidth: Int, maxHeight: Int, region: CGRect?,
                           histogram: Bool) -> DevelopRequest {
        DevelopRequest(
            maxWidth: UInt32(clamping: max(1, maxWidth)), maxHeight: UInt32(clamping: max(1, maxHeight)),
            region: region.map { NormRect(x: Float($0.minX), y: Float($0.minY), w: Float($0.width), h: Float($0.height)) },
            quality: ffiQuality(r.quality),
            priority: r.priority == .interactive ? .interactive : .background,
            format: ffiFormat(r.format), histogram: histogram,
            geometry: r.geometry == .final ? .final : .oriented)
    }

    /// The request for `r` given the frame's native size: fit renders use the long edge;
    /// region renders ask for the region's pixels at `scale` (≤ 1:1), the region widened
    /// (never narrowed) so its width is a multiple of `alignment` pixels (zero-copy rows).
    static func sized(_ r: RenderRequest, native: CGSize, alignment: Int = 1) -> (width: Int, height: Int, region: CGRect?) {
        guard var region = r.region, region.width > 0, region.height > 0, native.width > 0, native.height > 0 else {
            return (r.maxPixelSize, r.maxPixelSize, nil)
        }
        let s = min(max(r.scale ?? Double(r.maxPixelSize) / Double(max(native.width, native.height)), 0.01), 1)
        let scaledW = native.width * s
        var w = Int((region.width * scaledW).rounded(.up))
        let h = Int((region.height * native.height * s).rounded(.up))
        if alignment > 1, w % alignment != 0 {
            let aligned = w + alignment - w % alignment
            if CGFloat(aligned) <= scaledW.rounded(.down) {
                w = aligned
                let width = CGFloat(aligned) / scaledW
                region = CGRect(x: min(region.minX, 1 - width), y: region.minY, width: width, height: region.height)
            }
        }
        return (max(w, 1), max(h, 1), region)
    }

    static func mapping(_ p: DevelopPlan) -> FrameMapping {
        FrameMapping(affine: p.frameToCropSpace, nativeSize: CGSize(width: Int(p.nativeWidth), height: Int(p.nativeHeight)))
    }

    static func histogram(_ h: DevelopHistogram) -> Histogram {
        Histogram(red: h.red, green: h.green, blue: h.blue, luma: h.luma, shadowClipped: Int(h.clippedShadows),
                  highlightClipped: Int(h.clippedHighlights), pixelCount: Int(h.total))
    }

    static func aiTargets(in edit: EditState) -> Set<AITarget> {
        var out = Set<AITarget>()
        for layer in edit.layers where layer.enabled {
            for shape in layer.mask.shapes {
                if case .ai(_, let t) = shape { out.insert(t) }
            }
        }
        return out
    }

    // MARK: Rendering

    /// Plan `photo` with `edit` for `request` (decodes the RAW on first use; cached).
    public func plan(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> (DevelopPlan, DevelopRequest) {
        guard let url = photo.url else { throw MockError.notFound("file for \(photo.filename)") }
        let json = try AdjustmentsCodec.encode(edit)
        let path = url.path
        func planned(_ r: DevelopRequest) async throws -> DevelopPlan {
            try await Self.call { token in try await planDevelop(path: path, adjustmentsJson: json, request: r, token: token) }
        }
        let k = pixelAlignment(request.format)
        // The whole frame first: its native size (and mapping), and the fit size.
        var req = Self.ffiRequest(request, maxWidth: request.maxPixelSize, maxHeight: request.maxPixelSize,
                                  region: nil, histogram: request.histogram)
        var plan = try await planned(req)
        if request.region == nil {
            // Rows the GPU can texture in place (no copy): trim the width to the alignment.
            if Int(plan.width) % k != 0, Int(plan.width) > k {
                req.maxWidth = UInt32(Int(plan.width) - Int(plan.width) % k)
                plan = try await planned(req)
            }
        } else {
            let size = Self.sized(request, native: CGSize(width: Int(plan.nativeWidth), height: Int(plan.nativeHeight)),
                                  alignment: k)
            req = Self.ffiRequest(request, maxWidth: size.width, maxHeight: size.height, region: size.region,
                                  histogram: request.histogram)
            plan = try await planned(req)
        }
        return (plan, req)
    }

    /// Pixels per row alignment for a zero-copy linear texture of `format`.
    func pixelAlignment(_ format: FramePixelFormat) -> Int {
        guard let device else { return 1 }
        let tex = MetalViewerRenderer.textureFormat(for: format)
        return max(1, device.minimumLinearTextureAlignment(for: tex) / format.bytesPerPixel)
    }

    public func render(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> RenderedFrame {
        let signpost = Self.signposter.beginInterval("develop", id: Self.signposter.makeSignpostID(),
                                                     "\(String(describing: request.quality)) \(request.maxPixelSize)")
        defer { Self.signposter.endInterval("develop", signpost) }
        try Task.checkCancellation()
        guard let url = photo.url else { throw MockError.notFound("file for \(photo.filename)") }
        let (plan, req) = try await plan(photo, edit: edit, request: request)
        let json = try AdjustmentsCodec.encode(edit)
        let maskSource: DevelopMaskSource? = await StaticMasks.gather(edit, photo: photo.id, provider: maskProvider,
                                                                       persons: personMaskProvider)
        let length = Int(plan.bytesPerRow) * Int(plan.height)
        let allocationsBefore = pool.allocations
        let dest = try FrameDestination(length: length, device: device, pool: pool)
        // The viewer's photo stays decoded whatever else is decoded (exports, thumbnails).
        if request.priority == .interactive { pinDevelop(path: url.path) }
        let out: DevelopFrame
        do {
            let address = UInt64(UInt(bitPattern: dest.pointer))
            let sendableMasks = maskSource
            out = try await Self.call { token in
                try await renderDevelop(path: url.path, adjustmentsJson: json, request: req, destPtr: address,
                                        destLen: UInt64(dest.length), masks: sendableMasks, token: token)
            }
            try Task.checkCancellation()
        } catch {
            dest.discard()
            throw error
        }
        // Rows the GPU can't texture in place (unaligned) cost one GPU blit in the viewer.
        let alignedRows = Int(plan.bytesPerRow) % (pixelAlignment(request.format) * request.format.bytesPerPixel) == 0
        let gpu = Self.backend.gpu
        let allocated = pool.allocations - allocationsBefore
        lock.withLock {
            _stats.frames += 1
            _stats.bytes += length
            _stats.lastBytes = length
            _stats.allocations += allocated
            if gpu { _stats.rustCopies += 1 }
            if !alignedRows || !dest.isMetal { _stats.swiftCopies += 1 }
        }
        let mapping = request.geometry == .final ? Self.mapping(plan) : nil
        let region = req.region.map { CGRect(x: CGFloat($0.x), y: CGFloat($0.y), width: CGFloat($0.w), height: CGFloat($0.h)) }
        var full = CGSize(width: Int(out.width), height: Int(out.height))
        if let region, region.width > 0 {
            // The whole frame at this tile's scale.
            let s = Double(out.width) / (Double(region.width) * Double(plan.nativeWidth))
            full = CGSize(width: (Double(plan.nativeWidth) * s).rounded(), height: (Double(plan.nativeHeight) * s).rounded())
        }
        let geometry = RustRenderer.Geometry(width: Int(out.width), height: Int(out.height), bytesPerRow: Int(plan.bytesPerRow),
                                             format: request.format, region: region, fullSize: full)
        return dest.frame(geometry, mapping: mapping,
                          histogram: out.histogram.map(Self.histogram))
    }

    /// Warm the decode and GPU state for `photo` (a neighbour, or the pan slack around
    /// a 1:1 region) at background priority; returns the final frame's mapping so the
    /// viewer can place the preview in the develop render's geometry before it lands.
    @discardableResult
    public func prefetch(_ photo: Photo, edit: EditState, request: RenderRequest) async -> FrameMapping? {
        guard let url = photo.url, let json = try? AdjustmentsCodec.encode(edit) else { return nil }
        var r = request
        r.priority = .background
        let probe = Self.ffiRequest(r, maxWidth: r.maxPixelSize, maxHeight: r.maxPixelSize, region: nil, histogram: false)
        if r.region == nil {
            try? prefetchDevelop(path: url.path, adjustmentsJson: json, request: probe)
        }
        guard let plan = try? await Self.call({ token in
            try await planDevelop(path: url.path, adjustmentsJson: json, request: probe, token: token)
        }) else { return nil }
        if r.region != nil {
            // Pan slack around a 1:1 region: the BASE for the wider rectangle, in the background.
            let size = Self.sized(r, native: CGSize(width: Int(plan.nativeWidth), height: Int(plan.nativeHeight)))
            let req = Self.ffiRequest(r, maxWidth: size.width, maxHeight: size.height, region: size.region, histogram: false)
            try? prefetchDevelop(path: url.path, adjustmentsJson: json, request: req)
        }
        return r.geometry == .final ? Self.mapping(plan) : nil
    }

    /// Run an FFI call with a fresh token that follows Swift task cancellation.
    static func call<T>(_ body: (CancelToken) async throws -> T) async throws -> T {
        let token = try CancelToken()
        do {
            return try await withTaskCancellationHandler {
                try await body(token)
            } onCancel: {
                try? token.cancel()
            }
        } catch let e as DsError {
            if case .Cancelled = e { throw CancellationError() }
            throw RenderError(e)
        }
    }

    static let signposter = OSSignposter(subsystem: "app.luxtable.Luxtable", category: "render")
}

extension DevelopRenderer: PrefetchCancelling {
    /// Cancel neighbour prefetches for everything but `keep`: after navigation the
    /// old neighbourhood's decodes and BASE builds stop instead of holding the
    /// background slot.
    public func cancelPrefetches(keeping keep: [Photo]) {
        cancelDevelopPrefetches(keep: keep.compactMap { $0.url?.path })
    }
}

/// One-time GPU renderer creation off the main thread.
final class WarmState: @unchecked Sendable {
    private let lock = NSLock()
    private var task: Task<Void, Never>?
    private var ready = false

    var isReady: Bool { lock.withLock { ready } }

    func start() -> Task<Void, Never> {
        lock.withLock {
            if let task { return task }
            let t = Task.detached(priority: .userInitiated) { [self] in
                _ = try? developBackend() // creates the device and compiles the kernels
                lock.withLock { ready = true }
            }
            task = t
            return t
        }
    }
}

/// Page-aligned frame memory recycled between frames of the same size. Each frame gets
/// a lightweight `MTLBuffer` over a block (`bytesNoCopy`); when that buffer (and every
/// texture made from it) is released the block comes back here.
public final class FrameMemoryPool: @unchecked Sendable {
    private let lock = NSLock()
    private var free: [Int: [UnsafeMutableRawPointer]] = [:]
    private var _allocations = 0
    /// Blocks kept per size (frames on screen, before/after, a tile, one in flight).
    let keepPerSize: Int

    public init(keepPerSize: Int = 4) { self.keepPerSize = keepPerSize }

    /// Fresh blocks allocated so far.
    public var allocations: Int { lock.withLock { _allocations } }
    /// Blocks waiting for reuse.
    public var idle: Int { lock.withLock { free.values.reduce(0) { $0 + $1.count } } }

    struct Pooled {
        let buffer: MTLBuffer
        let pointer: UnsafeMutableRawPointer
    }

    static func rounded(_ length: Int) -> Int {
        let page = Int(getpagesize())
        return (max(length, 1) + page - 1) / page * page
    }

    func buffer(length: Int, device: MTLDevice) -> Pooled? {
        let size = Self.rounded(length)
        guard MetalLimits.canMakeBuffer(device, length: size) else { return nil }
        let block: UnsafeMutableRawPointer = lock.withLock {
            if let b = free[size]?.popLast() { return b }
            _allocations += 1
            return UnsafeMutableRawPointer.allocate(byteCount: size, alignment: Int(getpagesize()))
        }
        guard let b = device.makeBuffer(bytesNoCopy: block, length: size, options: .storageModeShared,
                                        deallocator: { [weak self] p, n in
                                            if let self { self.give(p, size: n) } else { p.deallocate() }
                                        }) else {
            give(block, size: size)
            return nil
        }
        return Pooled(buffer: b, pointer: block)
    }

    func give(_ p: UnsafeMutableRawPointer, size: Int) {
        let keep: Bool = lock.withLock {
            guard free[size, default: []].count < keepPerSize else { return false }
            free[size, default: []].append(p)
            return true
        }
        if !keep { p.deallocate() }
    }

    deinit {
        for blocks in free.values { blocks.forEach { $0.deallocate() } }
    }
}

/// The parts of an edit that change where the final frame's pixels come from
/// (crop, straighten, turns, lens and keystone): a cached `FrameMapping` stays valid
/// while these are unchanged.
struct GeometryKey: Equatable {
    var crop: CropEdit
    var lens: LensEdit

    init(_ e: EditState) {
        crop = e.crop
        lens = e.lens
    }
}

/// AI masks gathered before a render (the pipeline asks for them from a render thread).
final class StaticMasks: DevelopMaskSource, @unchecked Sendable {
    let masks: [AITarget: VisionMask]
    let persons: [PersonMaskKey: VisionMask]
    let depth: VisionMask?

    init(_ masks: [AITarget: VisionMask], persons: [PersonMaskKey: VisionMask] = [:], depth: VisionMask? = nil) {
        self.masks = masks
        self.persons = persons
        self.depth = depth
    }

    func depthMap() -> DevelopMask? {
        depth.map(DevelopMask.init)
    }

    func aiMask(target: DevelopAiTarget) -> DevelopMask? {
        masks[AITarget(target)].map(DevelopMask.init)
    }

    func aiPersonMask(target: DevelopAiTarget, person: UInt32) -> DevelopMask? {
        persons[PersonMaskKey(target: AITarget(target), person: Int(person))].map(DevelopMask.init)
    }
}
