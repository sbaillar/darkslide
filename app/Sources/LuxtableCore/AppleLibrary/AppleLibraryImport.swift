import Foundation

// Aperture and iPhoto library import (lt-apple through the FFI): File ▸ Import Aperture
// or iPhoto Library… reads an Aperture 3 library, an iPhoto library (any version from 6)
// and brings ratings, flags, labels, keywords, titles, captions, places, the edits that
// map, and projects, events and albums into the open session, the way the Lightroom
// import does (same engine, categories, dry run, conflicts and revert). The library is
// only ever read; a Photos library is recognised and refused with a message.

/// What the chosen path is (lt-apple's `Format`, by its wire key).
public enum AppleLibraryKind: String, Sendable, Equatable {
    case aperture, iphoto
    case iphotoXML = "iphoto-xml"
    case photos
    case oldAperture = "old-aperture"
    case unknown

    /// One Luxtable can import.
    public var isReadable: Bool { self == .aperture || self == .iphoto || self == .iphotoXML }

    /// Why it can't be imported, for the kinds that can't.
    public func refusal(_ name: String) -> String? {
        switch self {
        case .aperture, .iphoto, .iphotoXML: nil
        case .photos:
            "\u{201c}\(name)\u{201d} is a Photos library, which Luxtable doesn't read: only Aperture and iPhoto libraries are "
                + "supported. In Photos, export the unmodified originals (File \u{25B8} Export) and open that folder instead."
        case .oldAperture:
            "\u{201c}\(name)\u{201d} was written by Aperture 1 or 2, which Luxtable can't read. Open it once in Aperture 3 "
                + "to upgrade it, then choose it again."
        case .unknown:
            "\u{201c}\(name)\u{201d} isn't an Aperture or iPhoto library."
        }
    }
}

/// A loaded library, read once (the live store keeps the Rust object under `id`).
public struct AppleLibraryInfo: Sendable, Equatable {
    public var id = UUID()
    public var kind: AppleLibraryKind = .aperture
    public var origin = ""
    public var writtenBy: String?
    public var photos = 0
    /// Projects, events and albums that become Luxtable albums.
    public var albums = 0
    /// Smart albums, books, slideshows: listed, not imported.
    public var skippedAlbums = 0
    public var warnings: [String] = []

    public init(id: UUID = UUID(), kind: AppleLibraryKind = .aperture, origin: String = "", writtenBy: String? = nil,
                photos: Int = 0, albums: Int = 0, skippedAlbums: Int = 0, warnings: [String] = []) {
        self.id = id
        self.kind = kind
        self.origin = origin
        self.writtenBy = writtenBy
        self.photos = photos
        self.albums = albums
        self.skippedAlbums = skippedAlbums
        self.warnings = warnings
    }

    /// "Library.aplibrary (Aperture 3): 1200 photos, 14 albums".
    public var line: String {
        var parts = [LRImportSummary.photos(photos)]
        if albums > 0 { parts.append("\(albums) album\(albums == 1 ? "" : "s")") }
        let by = writtenBy.map { " (\($0))" } ?? ""
        return "\(origin)\(by): " + parts.joined(separator: ", ")
    }
}

/// The categories an Aperture or iPhoto library can bring over (lens corrections,
/// vignettes and local adjustments don't map from it).
public enum AppleImportCategories {
    public static let organise: [LRCategory] = [.rating, .label, .flag, .keywords, .metadata, .collections]
    public static let develop: [LRCategory] = [.crop, .tone, .whiteBalance, .color, .detail]
    public static var all: Set<LRCategory> { Set(organise + develop) }

    /// The checkbox's words for this import.
    public static func title(_ c: LRCategory) -> String {
        switch c {
        case .metadata: "Title, caption and place"
        case .collections: "Projects, events and albums"
        default: c.title
        }
    }
}

/// The store side of the import (the live store; demo mode has none).
public protocol AppleLibraryImporting: Sendable {
    /// What the path is (cheap: only its files are looked at).
    func appleLibraryKind(of url: URL) async -> AppleLibraryKind
    /// Read a library once; later calls refer to it by the returned id.
    func loadAppleLibrary(_ url: URL) async throws -> AppleLibraryInfo
    func releaseAppleLibrary(_ id: UUID) async
    func importAppleLibrary(_ session: SessionInfo, library: UUID, categories: Set<LRCategory>, dryRun: Bool,
                            overwriteEdits: Bool) async throws -> LRImportSummary
    func canRevertAppleImport(_ session: SessionInfo) async -> Bool
    /// Take back the imports since the last revert; `nil`: nothing to revert.
    func revertAppleImport(_ session: SessionInfo) async throws -> LRRevertSummary?
}

/// The open session's Aperture / iPhoto state: whether there is an import to revert.
public struct AppleLibraryStatus: Equatable, Sendable {
    public var session: UUID?
    public var canRevert = false
}
