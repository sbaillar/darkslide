import CoreGraphics
import DarkslideFFI
import Foundation

/// `DevelopTools` over ds-ffi `tools.rs` (the shared GPU renderer, CPU fallback).
public final class RustDevelopTools: DevelopTools, @unchecked Sendable {
    let thumbnails: DevelopThumbnails?
    private let lock = NSLock()
    /// As-shot/lens info per file version (a file replaced at the same path is read
    /// again, L2). Reading it is cheap (tags only, no decode) since H4.
    private var infos: [FileVersion: PhotoDevelopInfo] = [:]
    private var _maskProvider: DevelopRenderer.MaskProvider?
    private var _personMaskProvider: PersonMaskProvider?

    public var personMaskProvider: PersonMaskProvider? {
        get { lock.withLock { _personMaskProvider } }
        set { lock.withLock { _personMaskProvider = newValue } }
    }

    /// AI mask bitmaps for edited thumbnails with AI-mask layers (M5): the viewer's
    /// Vision provider.
    public var maskProvider: DevelopRenderer.MaskProvider? {
        get { lock.withLock { _maskProvider } }
        set { lock.withLock { _maskProvider = newValue } }
    }

    /// Path + size + modification date + inode: one version of a file.
    struct FileVersion: Hashable {
        let path: String
        let size: Int?
        let modified: Date?
        let inode: UInt64?

        init(_ url: URL) {
            path = url.path
            let a = try? FileManager.default.attributesOfItem(atPath: url.path)
            size = (a?[.size] as? NSNumber)?.intValue
            modified = a?[.modificationDate] as? Date
            inode = (a?[.systemFileNumber] as? NSNumber)?.uint64Value
        }
    }

    /// `thumbnailCache`: where edited thumbnails are kept (`nil` = no disk cache).
    public init(thumbnailCache: URL? = RustDevelopTools.defaultThumbnailCache, capBytes: UInt64 = 512 << 20) {
        thumbnails = thumbnailCache.flatMap { try? DevelopThumbnails(root: $0.path, capBytes: capBytes) }
    }

    /// `<AppPaths.support>/Thumbnails` (never the real folder in snapshot/test runs).
    public static var defaultThumbnailCache: URL { AppPaths.support.appendingPathComponent("Thumbnails", isDirectory: true) }

    public var diskCacheBytes: UInt64 { thumbnails?.totalBytes() ?? 0 }

    /// Run an FFI call with a cancel token wired to the Swift task.
    static func run<T>(_ body: (CancelToken) async throws -> T) async throws -> T {
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

    static func value(_ w: WbValue) -> WhiteBalanceValue {
        WhiteBalanceValue(kelvin: Double(w.kelvin), tint: Double(w.tint))
    }

    static func json(_ e: EditState) throws -> String { try AdjustmentsCodec.encode(e) }

    static func file(_ photo: Photo) throws -> URL {
        guard let url = photo.url else { throw DevelopToolsError.unavailable("\(photo.filename) has no file") }
        return url
    }

    public func info(for photo: Photo) async throws -> PhotoDevelopInfo {
        let url = try Self.file(photo)
        let version = FileVersion(url)
        if let hit = lock.withLock({ infos[version] }) { return hit }
        let i = try await Self.run { try await developInfo(path: url.path, token: $0) }
        let info = PhotoDevelopInfo(asShot: Self.value(i.asShot), lens: i.lens, lensProfile: i.lensProfile,
                                    iso: i.iso.map(Int.init), renderer: i.renderer)
        lock.withLock { infos[version] = info }
        return info
    }

    public func autoWhiteBalance(for photo: Photo) async throws -> WhiteBalanceValue {
        let url = try Self.file(photo)
        return Self.value(try await Self.run { try await DarkslideFFI.autoWhiteBalance(path: url.path, token: $0) })
    }

    public func autoTone(for photo: Photo, edit: EditState) async throws -> EditState {
        let url = try Self.file(photo)
        let json = try Self.json(edit)
        let out = try await Self.run { try await DarkslideFFI.autoTone(path: url.path, adjustmentsJson: json, token: $0) }
        let (toned, _) = try AdjustmentsCodec.decode(out)
        return edit.withTone(from: toned)
    }

    public func pickWhiteBalance(for photo: Photo, edit: EditState, at point: CGPoint, sample: Int) async throws -> WhiteBalanceValue {
        let url = try Self.file(photo)
        let json = try Self.json(edit)
        return Self.value(try await Self.run {
            try await DarkslideFFI.pickWhiteBalance(path: url.path, adjustmentsJson: json, x: Float(point.x),
                                                    y: Float(point.y), sample: UInt32(clamping: max(sample, 1)), token: $0)
        })
    }

    public func editedThumbnail(for photo: Photo, edit: EditState) async throws -> (image: CGImage, cached: Bool) {
        let url = try Self.file(photo)
        guard let thumbnails else { throw DevelopToolsError.unavailable("no edited-thumbnail cache") }
        let json = try Self.json(edit)
        let sendableMasks: DevelopMaskSource? = await StaticMasks.gather(edit, photo: photo.id, provider: maskProvider,
                                                                         persons: personMaskProvider)
        let t = try await Self.run {
            try await thumbnails.thumbnail(path: url.path, variant: UInt64(bitPattern: photo.id.raw), adjustmentsJson: json,
                                           masks: sendableMasks, token: $0)
        }
        guard let img = ThumbnailJPEG.image(t.jpeg) else { throw DevelopToolsError.unavailable("the thumbnail could not be decoded") }
        return (img, t.cached)
    }
}

public enum DevelopToolsError: Error, Equatable, LocalizedError {
    case unavailable(String)
    public var errorDescription: String? {
        switch self {
        case .unavailable(let why): "Not available: \(why)"
        }
    }
}
