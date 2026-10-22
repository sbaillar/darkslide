import CoreGraphics
import DarkslideFFI
import Foundation
import ImageIO

/// `PhotoMerging` over ds-ffi: `merge_preview` (paths) and `SessionHandle.merge_photos`,
/// which writes the merged DNG next to the sources and registers it as a new photo.
public struct RustPhotoMerger: PhotoMerging {
    let handles: RustArchiveService.HandleLookup

    /// `handles`: the open session's handle (the store's `archiveHandle(for:)`).
    public init(handles: @escaping RustArchiveService.HandleLookup) {
        self.handles = handles
    }

    public func preview(_ photos: [Photo], options: PhotoMergeOptions, maxEdge: Int) async throws -> PhotoMergePreview {
        let paths = try photos.map { p in
            guard let url = p.url else { throw RenderError(.InvalidArgument(message: "\(p.filename) has no file")) }
            return url.path
        }
        let ffiOptions = options.ffi
        let edge = UInt32(clamping: maxEdge)
        let p = try await RustSessionStore.call { token in
            try await mergePreview(paths: paths, options: ffiOptions, maxEdge: edge, token: token)
        }
        guard let image = Self.decode(p.jpeg) else {
            throw RenderError(.Internal(message: "the merge preview couldn't be decoded"))
        }
        return PhotoMergePreview(image: image, width: Int(p.width), height: Int(p.height),
                                 projection: PhotoMergeOptions.Projection(p.projection),
                                 ghostRegions: Int(p.ghostRegions), exposureEV: Double(p.exposureEv),
                                 unalignedFrames: Int(p.unalignedFrames))
    }

    public func merge(_ photos: [PhotoID], in session: SessionInfo, options: PhotoMergeOptions,
                      progress: @escaping @Sendable (Double) -> Void) async throws -> PhotoID {
        guard let h = await handles(session) else { throw MockError.notFound("session \(session.name)") }
        let stream = ProgressStream()
        let listen = Task {
            for await e in stream.events where e.kind == "merge" && e.total > 0 {
                progress(Double(e.completed) / Double(e.total))
            }
        }
        defer {
            stream.finish()
            listen.cancel()
        }
        let ids = photos.map(\.raw)
        let ffiOptions = options.ffi
        let outcome = try await RustSessionStore.call { token in
            try await h.mergePhotos(variantIds: ids, options: ffiOptions, listener: stream, token: token)
        }
        return PhotoID(outcome.variantId)
    }

    static func decode(_ jpeg: Data) -> CGImage? {
        guard let src = CGImageSourceCreateWithData(jpeg as CFData, nil) else { return nil }
        return CGImageSourceCreateImageAtIndex(src, 0, nil)
    }
}

extension PhotoMergeOptions {
    var ffi: MergeOptions {
        MergeOptions(kind: kind == .hdr ? .hdr : .panorama, align: align, deghost: deghost.ffi,
                     projection: projection.ffi, autoCrop: autoCrop)
    }
}

extension PhotoMergeOptions.Deghost {
    var ffi: DarkslideFFI.Deghost {
        switch self {
        case .off: .off
        case .low: .low
        case .medium: .medium
        case .high: .high
        }
    }
}

extension PhotoMergeOptions.Projection {
    var ffi: PanoProjection {
        switch self {
        case .auto: .auto
        case .spherical: .spherical
        case .cylindrical: .cylindrical
        case .perspective: .perspective
        }
    }

    init(_ p: PanoProjection) {
        self = switch p {
        case .auto: .auto
        case .spherical: .spherical
        case .cylindrical: .cylindrical
        case .perspective: .perspective
        }
    }
}
