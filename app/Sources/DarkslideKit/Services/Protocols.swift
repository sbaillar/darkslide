import CoreGraphics
import Foundation
import Metal

// Service seams between the UI and the Rust core. Each has a mock in `Mock/`
// and (as the crates land) a Rust-backed implementation in `Rust/`.
// All are `Sendable` and async: the UI calls them from `Task`s, never blocking
// the main thread. See docs/FFI.md for the Rust facade each one maps to.

// MARK: - Rendering

/// Pixel layout of a rendered frame (mirrors the FFI `FrameFormat`).
public enum FramePixelFormat: Sendable, Equatable {
    /// 8-bit sRGB-encoded RGBA, alpha 255. Display-referred SDR.
    case rgba8Srgb
    /// 8-bit RGBA, Display P3 primaries with the sRGB transfer function (wide-gamut SDR).
    case rgba8DisplayP3
    /// 16-bit float RGBA in linear extended Display P3 (values > 1 = EDR headroom).
    case rgba16fLinearP3
    /// 16-bit float RGBA in linear Rec.2020, the working gamut: colours outside P3 kept
    /// (the soft-proofing viewer).
    case rgba16fLinearRec2020

    public var bytesPerPixel: Int { isFloat ? 8 : 4 }
    /// 16-bit float, linear.
    public var isFloat: Bool { self == .rgba16fLinearP3 || self == .rgba16fLinearRec2020 }
    /// 8-bit, sRGB transfer (either primaries).
    public var isEightBit: Bool { !isFloat }
    /// Display P3 primaries.
    public var isP3: Bool { self == .rgba8DisplayP3 || self == .rgba16fLinearP3 }
}

/// Where a developed frame sits relative to crop space (`CropEdit.rect`, masks and
/// strokes: the oriented, lens/keystone-corrected image, normalized): an affine map
/// from frame-normalized points (0...1 across the whole final frame) to crop space,
/// and the final frame's size at 1:1 (ds-pipeline `frame_mapping`).
public struct FrameMapping: Sendable, Equatable {
    /// Row-major `[m00, m01, m02, m10, m11, m12]`.
    public var affine: [Double]
    /// Whole final frame at one output pixel per sensor pixel.
    public var nativeSize: CGSize

    public init(affine: [Double], nativeSize: CGSize) {
        precondition(affine.count == 6, "2x3 affine")
        self.affine = affine
        self.nativeSize = nativeSize
    }

    /// Frame-normalized -> crop space.
    public func toCrop(_ p: CGPoint) -> CGPoint {
        let m = affine
        return CGPoint(x: m[0] * p.x + m[1] * p.y + m[2], y: m[3] * p.x + m[4] * p.y + m[5])
    }

    /// Crop space -> frame-normalized (nil for a degenerate map).
    public func toFrame(_ p: CGPoint) -> CGPoint? {
        let m = affine
        let det = m[0] * m[4] - m[1] * m[3]
        guard abs(det) > 1e-15 else { return nil }
        let x = p.x - m[2], y = p.y - m[5]
        return CGPoint(x: (m[4] * x - m[1] * y) / det, y: (-m[3] * x + m[0] * y) / det)
    }
}

/// Where a frame's pixels live. `.buffer` is a Metal buffer Rust wrote into
/// directly (no copy across the FFI); `.data` is plain memory (tests, mocks, previews).
public enum FrameBacking: @unchecked Sendable {
    case data(Data)
    case buffer(MTLBuffer)
}

/// A rendered image or tile. Immutable once created (the backing is never
/// written after the render completes), hence `@unchecked Sendable`.
public struct RenderedFrame: @unchecked Sendable, Equatable {
    public let width: Int
    public let height: Int
    public let format: FramePixelFormat
    public let bytesPerRow: Int
    public let backing: FrameBacking
    /// Normalized region of the full image this frame covers (`nil` = whole image).
    public let region: CGRect?
    /// Pixel size of the whole image at the scale this frame was rendered at.
    /// Equals `(width, height)` for full-image frames; larger for tiles.
    public let fullSize: CGSize
    /// Monotonic identity (for cheap equality and texture caching).
    public let id: UInt64
    /// Set for developed frames of the final (cropped, straightened, turned) image: how
    /// the frame maps to crop space. Nil: the frame shows the oriented image itself.
    public let mapping: FrameMapping?
    /// The frame's display histogram when the renderer computed it with the pixels
    /// (ds-pipeline does it in the output pass): no second pass over the frame.
    public let histogram: Histogram?

    private static let counter = FrameCounter()

    public init(width: Int, height: Int, format: FramePixelFormat, bytesPerRow: Int, backing: FrameBacking,
                region: CGRect? = nil, fullSize: CGSize? = nil, mapping: FrameMapping? = nil,
                histogram: Histogram? = nil) {
        precondition(bytesPerRow >= width * format.bytesPerPixel, "row too short")
        self.width = width
        self.height = height
        self.format = format
        self.bytesPerRow = bytesPerRow
        self.backing = backing
        self.region = region
        self.fullSize = fullSize ?? CGSize(width: width, height: height)
        self.mapping = mapping
        self.histogram = histogram
        id = Self.counter.next()
        precondition(byteCount >= bytesPerRow * height, "pixel buffer too small")
    }

    /// 8-bit sRGB frame over plain memory.
    public init(width: Int, height: Int, rgba: Data) {
        self.init(width: width, height: height, format: .rgba8Srgb, bytesPerRow: width * 4, backing: .data(rgba))
    }

    public var byteCount: Int {
        switch backing {
        case .data(let d): d.count
        case .buffer(let b): b.length
        }
    }

    /// Borrow the pixels without copying.
    public func withBytes<R>(_ body: (UnsafeRawBufferPointer) throws -> R) rethrows -> R {
        switch backing {
        case .data(let d): return try d.withUnsafeBytes(body)
        case .buffer(let b): return try body(UnsafeRawBufferPointer(start: b.contents(), count: b.length))
        }
    }

    /// Copy of the pixels (tests and small frames only).
    public var rgba: Data { withBytes { Data($0) } }

    public static func == (a: RenderedFrame, b: RenderedFrame) -> Bool { a.id == b.id }

    /// CGImage view of an 8-bit frame (nil for float frames).
    public func makeCGImage() -> CGImage? {
        guard format.isEightBit else { return nil }
        guard let provider = CGDataProvider(data: rgba as CFData) else { return nil }
        let space = CGColorSpace(name: format == .rgba8DisplayP3 ? CGColorSpace.displayP3 : CGColorSpace.sRGB)!
        return CGImage(width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32,
                       bytesPerRow: bytesPerRow, space: space,
                       bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
                       provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
    }
}

final class FrameCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var value: UInt64 = 0
    func next() -> UInt64 {
        lock.withLock {
            value += 1
            return value
        }
    }
}

/// Develops a photo with an edit (→ `ds_pipeline::Renderer` via `Engine`).
public protocol Renderer: Sendable {
    /// Must return promptly with `CancellationError` (or `RenderError.cancelled`)
    /// when the calling task is cancelled; implementations forward cancellation to Rust.
    func render(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> RenderedFrame
}

/// Develop quality (ds-pipeline `Quality`).
public enum RenderQuality: Sendable, Equatable {
    /// Slider drags: developed at <= 2560 px and upscaled, no sharpening.
    case draft
    /// The settled view.
    case preview
    /// Full sensor demosaic (1:1 refine).
    case full
}

/// Scheduling class on the shared GPU (ds-pipeline `Priority`).
public enum RenderPriority: Sendable, Equatable {
    case interactive
    case background
}

/// Which image a develop renderer draws.
public enum RenderGeometry: Sendable, Equatable {
    /// The final output (cropped, straightened, turned): what export writes. Renderers
    /// that can (`RendererCapabilities.rendersGeometry`) return frames with a `mapping`.
    case final
    /// The oriented image with no crop/straighten/turns (what the crop, straighten and
    /// mask tools edit on): the viewer turns and straightens it itself.
    case oriented
}

/// What to render (mirrors the FFI `RenderRequest`, served by `ds-pipeline`).
public struct RenderRequest: Sendable, Equatable {
    /// Long edge of the *whole image* in output pixels (fit renders). Ignored when `scale` is set.
    public var maxPixelSize: Int
    /// Output pixels per sensor pixel (1.0 = true 100%). Set for zoomed/tiled renders.
    public var scale: Double?
    /// Normalized region of the image to render (the oriented image, or the final frame
    /// for `.final` geometry); `nil` = whole image.
    public var region: CGRect?
    public var format: FramePixelFormat
    public var quality: RenderQuality
    public var priority: RenderPriority
    public var geometry: RenderGeometry
    /// Return the frame's histogram with it (renderers that can; others ignore it).
    public var histogram: Bool

    public init(maxPixelSize: Int, scale: Double? = nil, region: CGRect? = nil, format: FramePixelFormat = .rgba8Srgb,
                quality: RenderQuality = .preview, priority: RenderPriority = .interactive,
                geometry: RenderGeometry = .oriented, histogram: Bool = false) {
        self.maxPixelSize = maxPixelSize
        self.scale = scale
        self.region = region
        self.format = format
        self.quality = quality
        self.priority = priority
        self.geometry = geometry
        self.histogram = histogram
    }
}

extension Renderer {
    /// Whole-image 8-bit render with the given long edge.
    public func render(_ photo: Photo, edit: EditState, maxPixelSize: Int) async throws -> RenderedFrame {
        try await render(photo, edit: edit, request: RenderRequest(maxPixelSize: maxPixelSize))
    }
}

/// Thumbnails for the browser (→ `ds_cache` via `Engine`). Must honour task cancellation.
public protocol ImageProvider: Sendable {
    func thumbnail(for photo: Photo, maxPixelSize: Int) async throws -> CGImage
}

/// Embedded JPEG previews straight from RAW files (→ `ds_raw::RawFile::preview` via FFI).
/// Instant: no develop, decoded with ImageIO off the main thread, oriented upright.
public protocol PreviewProvider: Sendable {
    /// `maxPixelSize` bounds the long edge (0 = full preview size, e.g. 1616 px on the A9).
    func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage
}

// MARK: - Sessions

/// Sessions, variants, ratings, and edits (→ `ds_session`).
public protocol SessionStore: Sendable {
    func listSessions() async throws -> [SessionInfo]
    func photos(in session: SessionInfo) async throws -> [Photo]
    func albums(in session: SessionInfo) async throws -> [Album]
    func createSession(named name: String) async throws -> SessionInfo
    /// Persist rating/tag/pick changes.
    func save(photos: [Photo]) async throws
    func edit(for id: PhotoID) async throws -> EditState
    /// Persist edits atomically (one undo step on the Rust side).
    func save(edits: [PhotoID: EditState]) async throws
    /// `false` when changes live only in memory (live mode before ds-session).
    var persistsChanges: Bool { get }
    /// Every non-identity edit in `session` (loaded when a session opens, N6).
    func edits(in session: SessionInfo) async throws -> [PhotoID: EditState]
}

extension SessionStore {
    public var persistsChanges: Bool { true }

    /// Default: ask per photo. Stores with a batch read should override.
    public func edits(in session: SessionInfo) async throws -> [PhotoID: EditState] {
        var out: [PhotoID: EditState] = [:]
        for p in try await photos(in: session) {
            let e = try await edit(for: p.id)
            if !e.isDefault { out[p.id] = e }
        }
        return out
    }
}

// MARK: - Import

/// What a card item is: photos can be developed; videos are imported as
/// non-editable items (play badge, Quick Look).
public enum ImportItemKind: String, Hashable, Sendable {
    case photo, video
}

public struct ImportCandidate: Identifiable, Hashable, Sendable {
    public var id: URL { url }
    public let url: URL
    public let size: Int64
    public let captureDate: Date
    /// Hash already present in the global index.
    public var alreadyImported: Bool
    public var kind: ImportItemKind
    /// Files that travel with it (the JPG of a RAW+JPG pair, XMP, THM).
    public var sidecars: [URL]
    /// Bytes of the item and its sidecars.
    public var totalBytes: Int64
    /// The library's copy of this file is damaged (no longer matches its hash):
    /// the file is imported again and the user is told.
    public var damagedLibraryCopy: URL?

    public init(url: URL, size: Int64, captureDate: Date, alreadyImported: Bool, kind: ImportItemKind = .photo,
                sidecars: [URL] = [], totalBytes: Int64? = nil, damagedLibraryCopy: URL? = nil) {
        self.url = url
        self.size = size
        self.captureDate = captureDate
        self.alreadyImported = alreadyImported
        self.kind = kind
        self.sidecars = sidecars
        self.totalBytes = totalBytes ?? size
        self.damagedLibraryCopy = damagedLibraryCopy
    }

    public var filename: String { url.lastPathComponent }
}

/// A card path and why it is listed (unreadable, or deliberately not imported).
public struct ImportIssue: Hashable, Sendable {
    public var url: URL
    public var reason: String

    public init(url: URL, reason: String) {
        self.url = url
        self.reason = reason
    }
}

/// Everything a card scan found. `scanErrors` (unreadable parts of the card)
/// mean an import can never be complete; `skippedUnknown` are card files that
/// are deliberately not imported. Both must be shown before an eject.
public struct ImportScan: Sendable, Equatable {
    public var candidates: [ImportCandidate]
    public var scanErrors: [ImportIssue]
    public var skippedUnknown: [ImportIssue]
    /// The card's identity (volume UUID) when the service knows it: a plan made
    /// from this scan only ever runs against the same card.
    public var volumeID: String?

    public init(candidates: [ImportCandidate], scanErrors: [ImportIssue] = [], skippedUnknown: [ImportIssue] = [],
                volumeID: String? = nil) {
        self.candidates = candidates
        self.scanErrors = scanErrors
        self.skippedUnknown = skippedUnknown
        self.volumeID = volumeID
    }
}

public enum ImportDestination: Hashable, Sendable {
    case newSession(name: String)
    case existing(SessionInfo)
}

public struct ImportPlan: Sendable, Equatable {
    public var candidates: [ImportCandidate]
    public var destination: ImportDestination
    public var renameTemplate: String
    public var backupFolder: URL?
    /// The card (or folder) the candidates come from.
    public var volume: URL?
    /// Subfolder template under `Capture/` (`""`: none).
    public var subfolderTemplate: String
    /// Re-read the card after copying (catches a flaky reader; doubles card reads).
    public var verifySourceReread: Bool
    public var skipDuplicates: Bool
    /// The session text (the `{text}` template token).
    public var sessionText: String
    /// The card the candidates were scanned from (`ImportScan.volumeID`).
    public var volumeID: String?

    public init(candidates: [ImportCandidate], destination: ImportDestination, renameTemplate: String, backupFolder: URL?,
                volume: URL? = nil, subfolderTemplate: String = "", verifySourceReread: Bool = false,
                skipDuplicates: Bool = true, sessionText: String = "", volumeID: String? = nil) {
        self.volumeID = volumeID
        self.candidates = candidates
        self.destination = destination
        self.renameTemplate = renameTemplate
        self.backupFolder = backupFolder
        self.volume = volume
        self.subfolderTemplate = subfolderTemplate
        self.verifySourceReread = verifySourceReread
        self.skipDuplicates = skipDuplicates
        self.sessionText = sessionText
    }
}

public struct ImportProgress: Sendable, Equatable {
    public var filesDone: Int
    public var filesTotal: Int
    public var bytesDone: Int64
    public var bytesTotal: Int64
    public var currentFile: String?

    public init(filesDone: Int, filesTotal: Int, bytesDone: Int64, bytesTotal: Int64, currentFile: String? = nil) {
        self.filesDone = filesDone
        self.filesTotal = filesTotal
        self.bytesDone = bytesDone
        self.bytesTotal = bytesTotal
        self.currentFile = currentFile
    }

    public var fraction: Double { bytesTotal == 0 ? 1 : Double(bytesDone) / Double(bytesTotal) }
}

/// Card scan and verified copy (→ `ds_import`).
public protocol ImportService: Sendable {
    func scan(volume: URL) async throws -> [ImportCandidate]
    /// The full scan: candidates plus what can't be (or isn't) imported.
    func scanCard(volume: URL) async throws -> ImportScan
    /// Copies and verifies; the stream finishes after the last file verifies.
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error>
}

extension ImportService {
    /// Services without a richer scan list only candidates.
    public func scanCard(volume: URL) async throws -> ImportScan {
        ImportScan(candidates: try await scan(volume: volume))
    }
}

// MARK: - Archive

public struct ArchiveStatus: Sendable, Equatable {
    public enum State: Sendable, Equatable {
        case idle
        /// `fraction` < 0: running without progress (openrsync reports none).
        case syncing(fraction: Double)
        case behind(files: Int)
        /// The last run left items out of the archive (unreadable originals,
        /// a failed DB snapshot): "Archived with problems", retried.
        case problems(count: Int)
        case offline
        /// The last run failed (`String`: why, in words; never rsync's raw output).
        case failed(String)
        /// A session was never archived (and nothing is pending or failed).
        case notArchived
        /// Archiving is off (no target, disabled) or not available in this mode.
        case unavailable(String)
    }

    public var state: State
    public var target: String
    public var lastSuccess: Date?
    /// Per-session status, recent runs and the Amazon hand-off (live mode).
    public var detail: ArchiveDetail?

    public init(state: State, target: String, lastSuccess: Date?, detail: ArchiveDetail? = nil) {
        self.state = state
        self.target = target
        self.lastSuccess = lastSuccess
        self.detail = detail
    }
}

/// What the archive popover lists (from ds-archive through the FFI).
public struct ArchiveDetail: Sendable, Equatable {
    public struct Session: Sendable, Equatable, Identifiable {
        public var id: String { source }
        public var name: String
        public var source: String
        /// Why it isn't archived (a folder on a card), if so.
        public var notArchivable: String?
        public var lastSuccess: Date?
        public var lastError: String?
        public var pending: Int
        public var behindSince: Date?
        public var running: Bool
        public var fraction: Double?
        public var bytesPerSecond: Double?
        public var retryIn: Duration?
        /// Not in the archive after the last run (`"<path>: <reason>"`).
        public var problems: [String]
        /// Its run stopped responding (stale mount) and was set aside.
        public var notResponding: Bool
        /// This session's own folder on the target.
        public var archiveFolder: String?
        /// This session's Amazon Photos hand-off.
        public var amazon: Amazon?
        /// The raw error behind `lastError` (rsync's output), for "Details".
        public var errorDetail: String?
        /// Originals at the last run, and the rejected ones it left out (N2).
        public var originals: Int
        public var rejectsExcluded: Int
        public init(name: String, source: String, notArchivable: String? = nil, lastSuccess: Date? = nil,
                    lastError: String? = nil, pending: Int = 0, behindSince: Date? = nil, running: Bool = false,
                    fraction: Double? = nil, bytesPerSecond: Double? = nil, retryIn: Duration? = nil,
                    problems: [String] = [], notResponding: Bool = false, archiveFolder: String? = nil,
                    amazon: Amazon? = nil, errorDetail: String? = nil, originals: Int = 0, rejectsExcluded: Int = 0) {
            self.errorDetail = errorDetail
            self.originals = originals
            self.rejectsExcluded = rejectsExcluded
            self.problems = problems
            self.notResponding = notResponding
            self.archiveFolder = archiveFolder
            self.amazon = amazon
            self.name = name
            self.source = source
            self.notArchivable = notArchivable
            self.lastSuccess = lastSuccess
            self.lastError = lastError
            self.pending = pending
            self.behindSince = behindSince
            self.running = running
            self.fraction = fraction
            self.bytesPerSecond = bytesPerSecond
            self.retryIn = retryIn
        }
    }

    public struct Run: Sendable, Equatable, Identifiable {
        public var id: String { "\(started.timeIntervalSince1970)-\(session)-\(outcome)" }
        public var session: String
        public var trigger: String
        /// "succeeded" | "incomplete" | "failed" | "offline" | "busy" |
        /// "cancelled" | "not responding"
        public var outcome: String
        public var message: String?
        public var started: Date
        public var duration: Duration
        public var warnings: [String]
        /// Items this run did not archive.
        public var problems: [String]
        /// The raw error (rsync's output) behind `message`, for "Details".
        public var detail: String?
        public init(session: String, trigger: String, outcome: String, message: String? = nil, started: Date,
                    duration: Duration, warnings: [String] = [], problems: [String] = [], detail: String? = nil) {
            self.detail = detail
            self.problems = problems
            self.session = session
            self.trigger = trigger
            self.outcome = outcome
            self.message = message
            self.started = started
            self.duration = duration
            self.warnings = warnings
        }
    }

    public struct Amazon: Sendable, Equatable {
        public var folder: String
        public var running: Bool
        public var lastRun: Date?
        public var placed: Int
        public var alreadyMirrored: Int
        /// Not hashed yet (handed over by a later run).
        public var waiting: Int
        public var failed: [String]
        public var error: String?
        /// The chosen folder is gone (moved, deleted, disk unplugged); it is never recreated.
        public var folderMissing: Bool
        /// Rejected originals not handed over (N2: off by default).
        public var rejectsExcluded: Int
        public init(folder: String, running: Bool = false, lastRun: Date? = nil, placed: Int = 0, alreadyMirrored: Int = 0,
                    waiting: Int = 0, failed: [String] = [], error: String? = nil, folderMissing: Bool = false,
                    rejectsExcluded: Int = 0) {
            self.rejectsExcluded = rejectsExcluded
            self.folderMissing = folderMissing
            self.folder = folder
            self.running = running
            self.lastRun = lastRun
            self.placed = placed
            self.alreadyMirrored = alreadyMirrored
            self.waiting = waiting
            self.failed = failed
            self.error = error
        }
    }

    public var targetReason: String?
    /// e.g. "/opt/homebrew/bin/rsync (rsync 3.5.1)".
    public var rsync: String?
    public var sessions: [Session]
    public var runs: [Run]
    public var amazon: Amazon?

    public init(targetReason: String? = nil, rsync: String? = nil, sessions: [Session] = [], runs: [Run] = [],
                amazon: Amazon? = nil) {
        self.targetReason = targetReason
        self.rsync = rsync
        self.sessions = sessions
        self.runs = runs
        self.amazon = amazon
    }
}

/// rsync archive scheduler (→ `ds_archive`). Every call returns quickly: the
/// work (and every file-system check of the target) runs in the background.
public protocol ArchiveService: Sendable {
    func statusUpdates() -> AsyncStream<ArchiveStatus>
    /// "Sync now": archive every open session, ignoring debounce and backoff.
    func syncNow() async
    /// `session` became the current session (it is scheduled from now on).
    func sessionOpened(_ session: SessionInfo) async
    /// `session` is closing: archive it now, then stop scheduling it.
    func sessionClosed(_ session: SessionInfo) async
    /// `n` files changed in `session` (ratings, tags, edits): archived after a quiet period.
    func noteChanges(_ n: Int, in session: SessionInfo) async
    /// **Import hook** (the import flow calls it once an import into `session`
    /// finished, with the number of files copied): archive the session now and
    /// mirror its originals into the Amazon Photos folder when one is set.
    func afterImport(session: SessionInfo, files: Int) async
    /// A volume was mounted or unmounted: check the target again now.
    func volumesChanged() async
    /// Quit: cancel a running rsync and the Amazon hand-off (bounded wait).
    func shutdown() async
}

extension ArchiveService {
    public func sessionOpened(_ session: SessionInfo) async {}
    public func sessionClosed(_ session: SessionInfo) async {}
    public func noteChanges(_ n: Int, in session: SessionInfo) async {}
    public func afterImport(session: SessionInfo, files: Int) async { await syncNow() }
    public func volumesChanged() async {}
    public func shutdown() async {}
}

// MARK: - AI

public struct CullSuggestion: Sendable, Equatable {
    public var photoID: PhotoID
    /// 0 (reject) ... 1 (keeper).
    public var score: Double
    public var pick: PickState
    /// Short human reasons, e.g. "Eyes closed", "Sharp subject".
    public var reasons: [String]

    public init(photoID: PhotoID, score: Double, pick: PickState, reasons: [String]) {
        self.photoID = photoID
        self.score = score
        self.pick = pick
        self.reasons = reasons
    }
}

/// On-device AI: culling, auto adjust, natural-language edits (→ Vision in Swift + `ds_ai`).
public protocol AIService: Sendable {
    func cull(_ photos: [Photo], images: any ImageProvider,
              progress: @escaping @Sendable (Int, Int) -> Void) async throws -> [CullSuggestion]
    func autoAdjust(_ photo: Photo, current: EditState, images: any ImageProvider) async throws -> EditState
    /// Apply a natural-language instruction ("warmer, lift shadows") to `current`.
    func applyInstruction(_ text: String, to current: EditState) async throws -> EditState
    /// [tl8] Same, knowing the photo's as-shot white balance (ds-ai `IntentContext.as_shot_wb`):
    /// a relative WB instruction on an As Shot photo starts from the real as-shot K/tint.
    func applyInstruction(_ text: String, to current: EditState, asShot: WhiteBalanceValue?) async throws -> EditState
}

extension AIService {
    public func applyInstruction(_ text: String, to current: EditState, asShot: WhiteBalanceValue?) async throws -> EditState {
        try await applyInstruction(text, to: current)
    }
}

// MARK: - Export

/// One photo to export.
public struct ExportJobItem: Sendable, Equatable {
    public var photoID: PhotoID
    public var url: URL
    /// `{seq}`: 1-based position in the export.
    public var seq: Int
    /// The photo's edits (developed by the pipeline); nil = the defaults.
    public var edit: EditState?
    /// The photo's keywords, written into the export's IPTC/XMP (with its metadata).
    public var keywords: [String]
    /// The photo's title, caption, credits…, written into the export's IPTC/XMP (with its
    /// metadata); its creator and copyright win over the job's artist and copyright.
    public var metadata: IPTCMetadata
    /// [map] Where it was taken (the geotag, else the file's EXIF, else XMP GPS):
    /// written as the export's position (unless the recipe removes the location).
    public var location: GeoPoint?

    public init(photoID: PhotoID, url: URL, seq: Int, edit: EditState? = nil, keywords: [String] = [],
                metadata: IPTCMetadata = IPTCMetadata(), location: GeoPoint? = nil) {
        self.photoID = photoID
        self.url = url
        self.seq = seq
        self.edit = edit
        self.keywords = keywords
        self.metadata = metadata
        self.location = location
    }
}

/// An export run: photos × enabled recipes into `defaultFolder` (recipes may override).
public struct ExportJob: Sendable, Equatable {
    public var items: [ExportJobItem]
    public var recipes: [ExportRecipe]
    public var defaultFolder: URL
    /// EXIF Artist / Copyright from Settings (nil: the original's own values, if any).
    public var artist: String?
    public var copyright: String?

    public init(items: [ExportJobItem], recipes: [ExportRecipe], defaultFolder: URL,
                artist: String? = nil, copyright: String? = nil) {
        self.items = items
        self.recipes = recipes
        self.defaultFolder = defaultFolder
        self.artist = artist
        self.copyright = copyright
    }
}

public struct ExportedOutput: Sendable, Equatable, Identifiable {
    public var id: URL { url }
    public var photoID: PhotoID
    public var recipeName: String
    public var url: URL
    public var width: Int
    public var height: Int
    public var bytes: Int64

    public init(photoID: PhotoID, recipeName: String, url: URL, width: Int, height: Int, bytes: Int64) {
        self.photoID = photoID
        self.recipeName = recipeName
        self.url = url
        self.width = width
        self.height = height
        self.bytes = bytes
    }
}

/// One export progress update: photos done of total; `readingMetadata` while
/// the originals' metadata is read (before the first render).
public struct ExportProgressUpdate: Sendable, Equatable {
    public var done: Int
    public var total: Int
    public var readingMetadata: Bool

    public init(done: Int, total: Int, readingMetadata: Bool = false) {
        self.done = done
        self.total = total
        self.readingMetadata = readingMetadata
    }
}

/// What `{name}`, `{camera}` and `{date}` give for a photo, read the way the
/// export reads them (the sheet's file-name example, E-L2).
public struct ExportFileNameSample: Sendable, Equatable {
    public var name: String
    public var camera: String?
    /// Camera wall clock, `yyyy-MM-ddTHH:mm:ss`.
    public var captureTime: String?
    /// The file's own EXIF credits (the export's fallback for `{artist}`/`{copyright}`).
    public var artist: String?
    public var copyright: String?

    public init(name: String, camera: String? = nil, captureTime: String? = nil, artist: String? = nil,
                copyright: String? = nil) {
        self.name = name
        self.camera = camera
        self.captureTime = captureTime
        self.artist = artist
        self.copyright = copyright
    }
}

/// The watermark preview's photo as sRGB RGBA8 (converted once per sheet).
public struct WatermarkPreviewPixels: Sendable, Equatable {
    public var width: Int
    public var height: Int
    public var rgba: Data

    public init(width: Int, height: Int, rgba: Data) {
        self.width = width
        self.height = height
        self.rgba = rgba
    }
}

/// A drawn watermark preview, or why there is none / what the export will do.
public struct WatermarkPreviewResult: @unchecked Sendable {
    public var image: CGImage?
    /// Shown under the preview: the text is blank for this photo (the export leaves
    /// the watermark out), or why the preview couldn't be drawn.
    public var message: String?

    public init(image: CGImage?, message: String? = nil) {
        self.image = image
        self.message = message
    }
}

public struct ExportOutcome: Sendable, Equatable {
    public var written: [ExportedOutput]
    /// Human-readable failures ("DSC0001 · Web: …").
    public var failures: [String]
    public var cancelled: Bool
    /// What developed the RAWs, e.g. "develop (Apple M4 Pro)".
    public var renderer: String
    /// "imageio" (hardware) or "mozjpeg".
    public var jpegEncoder: String
    /// Files written without their recipe's text watermark (its text was blank for the photo).
    public var watermarkSkipped: Int

    public init(written: [ExportedOutput], failures: [String], cancelled: Bool, renderer: String = "",
                jpegEncoder: String, watermarkSkipped: Int = 0) {
        self.written = written
        self.failures = failures
        self.cancelled = cancelled
        self.renderer = renderer
        self.jpegEncoder = jpegEncoder
        self.watermarkSkipped = watermarkSkipped
    }
}

/// Export through ds-export (→ `export_batch`).
public protocol ExportService: Sendable {
    /// First problem with `recipes` (bad template, quality, size), or nil.
    func validate(_ recipes: [ExportRecipe], defaultFolder: URL) -> String?
    /// The file name `template` gives the photo described by `sample` (live preview in the sheet).
    func previewFileName(template: String, sample: ExportFileNameSample, seq: Int, ext: String) -> String?
    /// Name, camera and capture time of the original at `url`, as the export reads them.
    func nameSample(for url: URL) async -> ExportFileNameSample?
    /// Run; `progress` per photo (and while metadata is read). Cancelling the task stops between outputs.
    func export(_ job: ExportJob, progress: @escaping @Sendable (ExportProgressUpdate) -> Void) async throws -> ExportOutcome
    /// `pixels` with `watermark` drawn as the export draws it (tokens from `sample`;
    /// `sample`'s credits are the ones the export uses). Blocking: call it off the main thread.
    func watermarkPreview(_ pixels: WatermarkPreviewPixels, watermark: ExportWatermark,
                          sample: ExportFileNameSample) -> WatermarkPreviewResult
}

public extension ExportService {
    func nameSample(for url: URL) async -> ExportFileNameSample? { nil }

    func watermarkPreview(_ pixels: WatermarkPreviewPixels, watermark: ExportWatermark,
                          sample: ExportFileNameSample) -> WatermarkPreviewResult {
        WatermarkPreviewResult(image: nil)
    }

    /// `progress(done, total)` for callers that only count photos.
    func export(_ job: ExportJob, progress: @escaping @Sendable (Int, Int) -> Void) async throws -> ExportOutcome {
        try await export(job) { (u: ExportProgressUpdate) in progress(u.done, u.total) }
    }
}
