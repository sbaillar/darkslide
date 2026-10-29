import CoreGraphics
import Foundation

/// Demo merges: a gradient preview and a new photo added to the mock session after the
/// first source (no files). `step` paces the four progress steps; cancelling stops it.
public struct MockPhotoMerger: PhotoMerging {
    let store: MockSessionStore
    let step: Duration

    public init(store: MockSessionStore, step: Duration = .milliseconds(250)) {
        self.store = store
        self.step = step
    }

    public func preview(_ photos: [Photo], options: PhotoMergeOptions, maxEdge: Int) async throws -> PhotoMergePreview {
        guard photos.count >= 2 else { throw MockError.invalid("Choose two or more photos to merge.") }
        try await Task.sleep(for: step)
        let (w, h) = Self.size(of: photos, options: options)
        let scale = min(1, Double(maxEdge) / Double(max(w, h, 1)))
        let image = try Self.gradient(width: max(1, Int(Double(w) * scale)), height: max(1, Int(Double(h) * scale)))
        let projection: PhotoMergeOptions.Projection = options.kind == .hdr ? .perspective
            : options.projection == .auto ? (photos.count > 6 ? .spherical : .cylindrical) : options.projection
        let blink = options.bestShot.contains { f in f.faces.contains { ($0.eyesOpen ?? 1) < 0.5 } }
        return PhotoMergePreview(image: image, width: w, height: h, projection: projection,
                                 ghostRegions: options.kind == .hdr && options.deghost != .off ? 2 : 0,
                                 exposureEV: options.kind == .hdr ? 0.5 : 0,
                                 facesReplaced: options.kind == .burst && blink ? 1 : 0,
                                 regionsReplaced: options.kind == .burst ? 1 : 0)
    }

    public func merge(_ photos: [PhotoID], in session: SessionInfo, options: PhotoMergeOptions,
                      progress: @escaping @Sendable (Double) -> Void) async throws -> PhotoID {
        let all = try await store.photos(in: session)
        let sources = photos.compactMap { id in all.first { $0.id == id } }
        guard sources.count >= 2, let first = sources.first else {
            throw MockError.invalid("Choose two or more photos to merge.")
        }
        for i in 1...4 {
            try await Task.sleep(for: step)
            progress(Double(i) / 4)
        }
        var meta = first.meta
        (meta.pixelWidth, meta.pixelHeight) = Self.size(of: sources, options: options)
        let base = (first.filename as NSString).deletingPathExtension
        let name = "\(base)-\(Self.tag(options.kind)).dng"
        return try await store.addPhoto(after: first.id, in: session, filename: name, meta: meta).id
    }

    /// The file name's tag, as lt-ffi names merges.
    static func tag(_ kind: PhotoMergeKind) -> String {
        switch kind {
        case .hdr: "HDR"
        case .panorama: "Pano"
        case .burst: "Best"
        }
    }

    /// HDR and a Best Shot keep the frame; a panorama about 80% of the frames side by side
    /// (auto crop: a bit less).
    static func size(of photos: [Photo], options: PhotoMergeOptions) -> (Int, Int) {
        let w = photos.first?.meta.pixelWidth ?? 0
        let h = photos.first?.meta.pixelHeight ?? 0
        guard options.kind == .panorama else { return (w, h) }
        let wide = Int(Double(photos.reduce(0) { $0 + $1.meta.pixelWidth }) * 0.8)
        return options.autoCrop ? (wide * 95 / 100, h * 9 / 10) : (wide, h)
    }

    static func gradient(width: Int, height: Int) throws -> CGImage {
        guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue),
              let gradient = CGGradient(colorsSpace: ctx.colorSpace,
                                        colors: [CGColor(red: 0.95, green: 0.6, blue: 0.3, alpha: 1),
                                                 CGColor(red: 0.2, green: 0.35, blue: 0.6, alpha: 1)] as CFArray,
                                        locations: [0, 1])
        else { throw MockError.invalid("no preview") }
        ctx.drawLinearGradient(gradient, start: .zero, end: CGPoint(x: 0, y: height), options: [])
        guard let image = ctx.makeImage() else { throw MockError.invalid("no preview") }
        return image
    }
}
