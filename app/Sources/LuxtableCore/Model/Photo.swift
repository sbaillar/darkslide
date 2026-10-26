import CoreGraphics
import Foundation

/// Stable identifier of a variant within a session (mirrors `lt-session`'s variant id).
public struct PhotoID: Hashable, Comparable, Sendable, Codable, CustomStringConvertible {
    public let raw: Int64
    public init(_ raw: Int64) { self.raw = raw }
    public static func < (a: PhotoID, b: PhotoID) -> Bool { a.raw < b.raw }
    public var description: String { "#\(raw)" }
}

/// Color labels: all seven of `lt_core::ColorTag` (R3-L3). Keys follow Lightroom
/// muscle memory: 6 red, 7 yellow, 8 green, 9 blue, - purple; orange and pink have
/// no default key (bindable). Raw values are append-only.
public enum ColorTag: Int, CaseIterable, Sendable, Codable {
    case none, red, yellow, green, blue, purple, orange, pink

    public var name: String {
        switch self {
        case .none: "None"
        case .red: "Red"
        case .yellow: "Yellow"
        case .green: "Green"
        case .blue: "Blue"
        case .purple: "Purple"
        case .orange: "Orange"
        case .pink: "Pink"
        }
    }

    /// The key that toggles this tag (`nil` for `.none`).
    public var key: Character? {
        switch self {
        case .none: nil
        case .red: "6"
        case .yellow: "7"
        case .green: "8"
        case .blue: "9"
        case .purple: "-"
        case .orange, .pink: nil
        }
    }

    public static func forKey(_ key: Character) -> ColorTag? {
        allCases.first { $0.key == key }
    }
}

public enum PickState: Int, Sendable, Codable, CaseIterable {
    case none, picked, rejected
}

/// Capture metadata (from `lt_raw::read_metadata` once wired).
public struct PhotoMeta: Hashable, Sendable {
    public var camera: String
    public var lens: String
    public var iso: Int
    /// Seconds.
    public var shutter: Double
    public var aperture: Double
    /// Millimetres.
    public var focalLength: Double
    public var captureDate: Date
    public var pixelWidth: Int
    public var pixelHeight: Int
    public var fileSize: Int64
    /// Sony `ShotNumberSincePowerUp` (orders frames inside a burst; lt-ai `GroupItem.seq`).
    public var shotNumber: Int?
    /// Sequence number from the file name (e.g. DSC04212 → 4212), fallback burst order.
    public var fileSequence: Int?
    /// Where the camera focused, oriented normalized coordinates (lt-ai `VisionSignals.af_point`).
    public var afPoint: CGPoint?
    /// [map] Where the file says the photo was taken: EXIF GPS, else XMP (once scanned).
    public var gps: GeoPoint?

    public init(camera: String, lens: String, iso: Int, shutter: Double, aperture: Double,
                focalLength: Double, captureDate: Date, pixelWidth: Int, pixelHeight: Int, fileSize: Int64) {
        self.camera = camera
        self.lens = lens
        self.iso = iso
        self.shutter = shutter
        self.aperture = aperture
        self.focalLength = focalLength
        self.captureDate = captureDate
        self.pixelWidth = pixelWidth
        self.pixelHeight = pixelHeight
        self.fileSize = fileSize
    }

    /// Camera wall-clock capture time as ms since the epoch *read as UTC* (the
    /// lt-session `capture_time_ms` convention); `nil` when unknown (then derived
    /// from `captureDate` in the local time zone).
    public var captureWallClockMs: Int64?

    /// Wall-clock capture time for date filters (same convention as lt-session).
    public var wallClockMs: Int64 { captureWallClockMs ?? SmartCriteria.wallClockMs(captureDate) }

    public var isPortrait: Bool { pixelHeight > pixelWidth }
    public var aspectRatio: Double { pixelHeight == 0 ? 1.5 : Double(pixelWidth) / Double(pixelHeight) }
}

/// One variant as the browser sees it.
public struct Photo: Identifiable, Hashable, Sendable {
    public let id: PhotoID
    public var filename: String
    public var meta: PhotoMeta
    /// 0 (unrated) ... 5.
    public var rating: Int
    public var colorTag: ColorTag
    /// Changing the flag drops the AI Cull note: it no longer explains it.
    public var pick: PickState {
        didSet { if pick != oldValue { cullNote = nil } }
    }
    /// Why AI Cull rejected it (sidecar `cull_note`), e.g. "Eyes closed". Set
    /// after `pick`; only while the AI's reject stands.
    public var cullNote: String?
    public var isEdited: Bool
    /// The original on disk (`nil` for synthetic demo photos).
    public var url: URL?
    /// The original is gone from where the session expects it (shown under the "Missing" filter).
    public var isMissing = false
    /// The file at this path was replaced by a different photo; metadata was not carried over
    /// (persistent until restored or kept as new).
    public var fileChanged = false
    /// Missing, but its only copy is still inside the session store (restorable to the folder).
    public var copyInStore = false
    /// Keywords (sidecar; written into exports as IPTC/XMP subjects).
    public var keywords: [String] = []
    /// Title, caption, credits, location and rights (sidecar; IPTC/XMP in exports).
    public var metadata = IPTCMetadata()
    /// [find] People tagged on it (sidecar), named as in the people store.
    public var people: [String] = []
    /// [cul9] H7: in the session's `Trash/` folder ("Move Rejected to Trash"):
    /// hidden from the browser, recoverable until "Delete Rejected…".
    public var inTrash = false
    /// [cul9] H7: in the session's `Selects/` folder.
    public var inSelects = false
    /// [cul9] H7: the variant's name ("" for the first; "Variant 2" for a new variant).
    public var variantName = ""
    /// [cul9] M2: the RAW is damaged or incomplete (the embedded preview may still
    /// show, but it can't be developed): badged in the browser.
    public var isDamaged = false
    /// [map] Where it was taken, set by hand on the map (sidecar; the original is never written).
    public var geotag: GeoPoint?

    /// [map] Where it was taken: the geotag, else the file's GPS (the one order everywhere).
    public var location: GeoPoint? { geotag ?? meta.gps }

    public init(id: PhotoID, filename: String, meta: PhotoMeta, rating: Int = 0,
                colorTag: ColorTag = .none, pick: PickState = .none, isEdited: Bool = false, url: URL? = nil) {
        self.id = id
        self.url = url
        self.filename = filename
        self.meta = meta
        self.rating = rating
        self.colorTag = colorTag
        self.pick = pick
        self.isEdited = isEdited
    }
}

// [imp6] Video clips imported from cards: shown with a play badge, opened in
// Quick Look, never developed, copied onto, or scored by AI.
extension Photo {
    /// lt-import's video extensions (`FileKind::Video`).
    public static let videoExtensions: Set<String> = ["mp4", "mts", "m2ts", "mov", "mxf", "m4v", "avi"]

    public static func isVideo(_ url: URL) -> Bool { videoExtensions.contains(url.pathExtension.lowercased()) }

    public var isVideo: Bool { url.map(Self.isVideo) ?? Self.isVideo(URL(fileURLWithPath: filename)) }
}

public struct SessionInfo: Identifiable, Hashable, Sendable {
    public let id: UUID
    public var name: String
    public var url: URL
    public var photoCount: Int
    public var createdAt: Date
    /// Why the folder can't be opened as it is: listed greyed out, never reopened on
    /// its own. `nil`: fine.
    public var unavailable: Unavailable?
    /// When the folder was last opened (recents; nil: not known).
    public var lastOpened: Date?

    /// Why a recent folder isn't usable as it is.
    public enum Unavailable: Hashable, Sendable {
        /// The folder isn't there (deleted, moved, or its drive or card isn't connected).
        case notFound
        /// The folder is there but holds no photos (only Luxtable's `.lt` is left).
        case noPhotos
        /// The folder's drive (an external disk, card or share) isn't mounted.
        case driveNotConnected

        /// The label shown in place of the photo count.
        public var label: String {
            switch self {
            case .notFound: "not found"
            case .noPhotos: "no photos found"
            case .driveNotConnected: "drive not connected"
            }
        }

        /// The folder can't be reached at all (gone, or its drive isn't connected).
        public var folderMissing: Bool { self != .noPhotos }
    }

    public init(id: UUID = UUID(), name: String, url: URL, photoCount: Int, createdAt: Date, unavailable: Unavailable? = nil) {
        self.id = id
        self.name = name
        self.url = url
        self.photoCount = photoCount
        self.createdAt = createdAt
        self.unavailable = unavailable
    }
}
