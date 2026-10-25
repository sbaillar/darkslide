import Foundation

// Lightroom Classic / Camera Raw import (ds-lr through the FFI): File ▸ Import
// Lightroom… reads a catalog (.lrcat), a folder of XMP sidecars or presets. The
// source is only ever read (a catalog is copied and the copy read, once); a dry
// run comes first, per-category checkboxes choose what is brought over, and
// Darkslide edits made since the last import are only replaced when the user
// says so. File ▸ Revert Lightroom Import takes the imports back.

/// What the picked path is.
public enum LRSourceKind: String, Sendable, Equatable {
    case catalog, sidecars, presets, unknown
}

/// One checkbox of the import dialog (ds-lr's `Category`, by its wire key).
public enum LRCategory: String, CaseIterable, Sendable, Hashable {
    case rating, label, flag, keywords, metadata, collections
    case crop, tone, whiteBalance = "white_balance", color, detail, effects, lens, masks

    public var title: String {
        switch self {
        case .rating: "Rating"
        case .label: "Colour label"
        case .flag: "Pick / reject flag"
        case .keywords: "Keywords"
        case .metadata: "Title, caption and copyright"
        case .collections: "Collections"
        case .crop: "Crop and straighten"
        case .tone: "Tone"
        case .whiteBalance: "White balance"
        case .color: "Colour"
        case .detail: "Detail"
        case .effects: "Vignette and grain"
        case .lens: "Lens corrections"
        case .masks: "Local adjustments"
        }
    }

    public var isDevelop: Bool {
        switch self {
        case .crop, .tone, .whiteBalance, .color, .detail, .effects, .lens, .masks: true
        default: false
        }
    }

    public static var organise: [LRCategory] { allCases.filter { !$0.isDevelop } }
    public static var develop: [LRCategory] { allCases.filter(\.isDevelop) }

    /// What the import does to a photo of this category, for the summary lines.
    var verb: String {
        switch self {
        case .rating: "rated"
        case .label: "colour-labelled"
        case .flag: "picked or rejected"
        case .keywords: "given keywords"
        case .metadata: "given a title, caption or copyright"
        case .collections: "added to collections"
        case .crop: "cropped or straightened"
        case .tone: "given tone edits"
        case .whiteBalance: "given a white balance"
        case .color: "given colour edits"
        case .detail: "given detail edits"
        case .effects: "given a vignette or grain"
        case .lens: "given lens corrections"
        case .masks: "given local adjustments"
        }
    }
}

/// A loaded source: a catalog or a folder of sidecars, read once (the live store keeps the
/// Rust object under `id`).
public struct LRSourceInfo: Sendable, Equatable {
    public var id = UUID()
    public var kind: LRSourceKind = .catalog
    public var origin = ""
    public var writtenBy: String?
    public var photos = 0
    public var collections = 0
    public var smartCollections = 0
    public var warnings: [String] = []

    public init(id: UUID = UUID(), kind: LRSourceKind = .catalog, origin: String = "", writtenBy: String? = nil,
                photos: Int = 0, collections: Int = 0, smartCollections: Int = 0, warnings: [String] = []) {
        self.id = id
        self.kind = kind
        self.origin = origin
        self.writtenBy = writtenBy
        self.photos = photos
        self.collections = collections
        self.smartCollections = smartCollections
        self.warnings = warnings
    }

    /// "Catalog.lrcat: 1200 photos, 14 collections".
    public var line: String {
        var parts = [LRImportSummary.photos(photos)]
        if collections > 0 { parts.append("\(collections) collection\(collections == 1 ? "" : "s")") }
        let by = writtenBy.map { " (Lightroom \($0))" } ?? ""
        return "\(origin)\(by): " + parts.joined(separator: ", ")
    }
}

/// What a Lightroom import did, or would do (a dry run). Mirrors ds-lr's report.
public struct LRImportSummary: Sendable, Equatable {
    public struct Mapped: Sendable, Equatable {
        public var key: String
        public var approximate: Bool
        public var count: Int
        public init(key: String, approximate: Bool = false, count: Int) {
            self.key = key
            self.approximate = approximate
            self.count = count
        }
    }

    public struct Dropped: Sendable, Equatable {
        public var key: String
        public var reason: String
        public var count: Int
        public init(key: String, reason: String, count: Int) {
            self.key = key
            self.reason = reason
            self.count = count
        }
    }

    public struct Conflict: Sendable, Equatable {
        public var photo: PhotoID
        public var file: String
        public var field: String
        public init(photo: PhotoID, file: String, field: String) {
            self.photo = photo
            self.file = file
            self.field = field
        }
    }

    public struct Skipped: Sendable, Equatable {
        public var name: String
        public var reason: String
        public init(name: String, reason: String) {
            self.name = name
            self.reason = reason
        }
    }

    public struct Note: Sendable, Equatable {
        public var file: String
        public var notes: [String]
        public init(file: String, notes: [String]) {
            self.file = file
            self.notes = notes
        }
    }

    public struct Folder: Sendable, Equatable {
        public var path: String
        public var photos: Int
        public init(path: String, photos: Int) {
            self.path = path
            self.photos = photos
        }
    }

    public var dryRun = true
    public var writtenBy: String?
    public var photos = 0
    public var matched = 0
    public var matchedByPath = 0
    public var matchedByHash = 0
    public var matchedByName = 0
    public var ambiguous: [String] = []
    /// The first photos with no Darkslide photo (the count is `notFoundCount`).
    public var notFound: [String] = []
    public var notFoundCount = 0
    public var notFoundFolders: [Folder] = []
    public var variantsCreated = 0
    /// Photos written per category key.
    public var changed: [String: Int] = [:]
    public var mapped: [Mapped] = []
    public var dropped: [Dropped] = []
    public var withDropped = 0
    public var unmappedLabels: [String: Int] = [:]
    public var inSync = 0
    public var keptDarkslide = 0
    public var deletedInDarkslide = 0
    public var conflicts: [Conflict] = []
    public var overwritten = 0
    public var readOnly = 0
    public var albumsCreated = 0
    public var albumsUpdated = 0
    public var skipped: [Skipped] = []
    public var notes: [Note] = []
    public var warnings: [String] = []
    /// The application the photos come from, as the lines name it.
    public var source = "Lightroom"

    public init(dryRun: Bool = true, writtenBy: String? = nil, photos: Int = 0, matched: Int = 0, matchedByPath: Int = 0,
                matchedByHash: Int = 0, matchedByName: Int = 0, ambiguous: [String] = [], notFound: [String] = [],
                notFoundCount: Int? = nil, notFoundFolders: [Folder] = [], variantsCreated: Int = 0,
                changed: [String: Int] = [:], mapped: [Mapped] = [], dropped: [Dropped] = [], withDropped: Int = 0,
                unmappedLabels: [String: Int] = [:], inSync: Int = 0, keptDarkslide: Int = 0, deletedInDarkslide: Int = 0,
                conflicts: [Conflict] = [], overwritten: Int = 0, readOnly: Int = 0, albumsCreated: Int = 0,
                albumsUpdated: Int = 0, skipped: [Skipped] = [], notes: [Note] = [], warnings: [String] = []) {
        self.dryRun = dryRun
        self.writtenBy = writtenBy
        self.photos = photos
        self.matched = matched
        self.matchedByPath = matchedByPath
        self.matchedByHash = matchedByHash
        self.matchedByName = matchedByName
        self.ambiguous = ambiguous
        self.notFound = notFound
        self.notFoundCount = notFoundCount ?? notFound.count
        self.notFoundFolders = notFoundFolders
        self.variantsCreated = variantsCreated
        self.changed = changed
        self.mapped = mapped
        self.dropped = dropped
        self.withDropped = withDropped
        self.unmappedLabels = unmappedLabels
        self.inSync = inSync
        self.keptDarkslide = keptDarkslide
        self.deletedInDarkslide = deletedInDarkslide
        self.conflicts = conflicts
        self.overwritten = overwritten
        self.readOnly = readOnly
        self.albumsCreated = albumsCreated
        self.albumsUpdated = albumsUpdated
        self.skipped = skipped
        self.notes = notes
        self.warnings = warnings
    }

    /// Changes the import writes (or would write).
    public var changeCount: Int {
        changed.values.reduce(0, +) + albumsCreated + albumsUpdated
    }

    /// Anything to import or decide.
    public var hasWork: Bool { changeCount > 0 || !conflicts.isEmpty }

    /// Photos with an edit the user made in Darkslide that the import would replace.
    public var conflictPhotos: Int { Set(conflicts.map(\.photo)).count }

    public func count(_ c: LRCategory) -> Int { changed[c.rawValue] ?? 0 }

    /// "812 of 1200 photos are in this session (by path 800, by content 10, by name 2)".
    public var matchLine: String {
        var how: [String] = []
        if matchedByPath > 0 { how.append("by path \(matchedByPath)") }
        if matchedByHash > 0 { how.append("by content \(matchedByHash)") }
        if matchedByName > 0 { how.append("by name and capture time \(matchedByName)") }
        let tail = how.isEmpty ? "" : " (\(how.joined(separator: ", ")))"
        return "\(matched) of \(Self.photos(photos)) matched a photo in this session\(tail)"
    }

    /// What is (or will be) brought over, one line each; zero counts are left out.
    public var lines: [String] {
        let verb = dryRun ? "will be" : "were"
        var out: [String] = []
        for c in LRCategory.allCases where c != .collections {
            let n = count(c)
            if n > 0 { out.append("\(Self.photos(n)) \(verb) \(c.verb)") }
        }
        if variantsCreated > 0 {
            let was = dryRun ? "will be" : variantsCreated == 1 ? "was" : "were"
            out.append("\(variantsCreated) virtual cop\(variantsCreated == 1 ? "y" : "ies") \(was) added as new variants")
        }
        let albums = albumsCreated + albumsUpdated
        if albums > 0 {
            let was = dryRun ? "will be" : albums == 1 ? "was" : "were"
            out.append("\(albums) album\(albums == 1 ? "" : "s") \(was) created or updated")
        }
        if out.isEmpty { out.append(conflicts.isEmpty ? "Everything is already up to date." : "Nothing new apart from the conflicts below.") }
        return out
    }

    /// The Darkslide tool a Lightroom setting lands in.
    static func tool(for key: String) -> String {
        switch key {
        case "Exposure2012": return "Exposure"
        case "Contrast2012": return "Contrast"
        case "Highlights2012": return "Highlights"
        case "Shadows2012": return "Shadows"
        case "Whites2012": return "Whites"
        case "Blacks2012": return "Blacks"
        case "Clarity2012": return "Clarity"
        case "Temperature", "Tint", "WhiteBalance": return "White balance"
        case "LuminanceSmoothing", "ColorNoiseReduction": return "Noise reduction"
        case "AutoLateralCA": return "Chromatic aberration"
        default: break
        }
        let prefixes: [(String, String)] = [
            ("HueAdjustment", "HSL"), ("SaturationAdjustment", "HSL"), ("LuminanceAdjustment", "HSL"),
            ("ToneCurve", "Tone curve"), ("Crop", "Crop"), ("Sharp", "Sharpening"), ("PostCropVignette", "Vignette"),
            ("Grain", "Grain"), ("LensProfile", "Lens corrections"), ("SplitToning", "Colour grading"),
            ("ColorGrade", "Colour grading"), ("Mask", "Local adjustments"), ("Local", "Local adjustments"),
            ("GradientBased", "Local adjustments"), ("CircularGradient", "Local adjustments"), ("PaintBased", "Local adjustments"),
        ]
        return prefixes.first { key.hasPrefix($0.0) }?.1 ?? key
    }

    /// Mapped settings per Darkslide tool, most photos first: "Exposure: 149 photos (approximate)".
    /// A tool fed by several settings counts its most frequent one.
    public var toolLines: [String] {
        var counts: [String: (n: Int, approx: Bool)] = [:]
        for m in mapped {
            let t = Self.tool(for: m.key)
            let old = counts[t]
            counts[t] = (max(old?.n ?? 0, m.count), (old?.approx ?? false) || m.approximate)
        }
        return counts.sorted { $0.value.n != $1.value.n ? $0.value.n > $1.value.n : $0.key < $1.key }
            .map { "\($0.key): \(Self.photos($0.value.n))\($0.value.approx ? " (approximate)" : "")" }
    }

    /// Settings that have no Darkslide translation: "Dehaze: 149 photos — reason".
    public var notMappedLines: [String] {
        dropped.map { "\($0.key): \(Self.photos($0.count)) — \($0.reason)" }
    }

    /// Darkslide edits the import would replace (with the overwrite option), one line per photo.
    public var conflictLines: [String] {
        var fields: [String: [String]] = [:]
        var files: [String] = []
        for c in conflicts {
            if fields[c.file] == nil { files.append(c.file) }
            fields[c.file, default: []].append(Self.fieldName(c.field))
        }
        let shown = files.prefix(8).map { "\($0): \(fields[$0]!.joined(separator: ", "))" }
        return files.count > 8 ? shown + ["and \(Self.photos(files.count - 8)) more"] : shown
    }

    static func fieldName(_ f: String) -> String {
        if let c = LRCategory(rawValue: f) { return c.title.lowercased() }
        return f
    }

    /// Photos and collections left out, and why.
    public var leftOutLines: [String] {
        var out: [String] = []
        if notFoundCount > 0 {
            out.append("\(Self.photos(notFoundCount)) not in this session: \(Self.sample(notFound.map { ($0 as NSString).lastPathComponent }))")
        }
        if !ambiguous.isEmpty {
            out.append("\(Self.photos(ambiguous.count)) could be several Darkslide photos (not imported): \(Self.sample(ambiguous.map { ($0 as NSString).lastPathComponent }))")
        }
        for (name, n) in unmappedLabels.sorted(by: { $0.key < $1.key }) {
            out.append("Label \u{201c}\(name)\u{201d} has no Darkslide colour: \(Self.photos(n)) left unlabelled")
        }
        if deletedInDarkslide > 0 { out.append("\(deletedInDarkslide) variant\(deletedInDarkslide == 1 ? "" : "s") deleted in Darkslide (not recreated)") }
        if readOnly > 0 { out.append("\(Self.photos(readOnly)) saved by a newer Darkslide (left alone)") }
        if keptDarkslide > 0 { out.append("\(keptDarkslide) Darkslide change\(keptDarkslide == 1 ? "" : "s") kept (unchanged in \(source) since the last import)") }
        out += skipped.map { "\u{201c}\($0.name)\u{201d}: \($0.reason)" }
        out += warnings
        return out
    }

    /// Per-photo notes (approximate or dropped settings, conflicts), one line per photo.
    public var noteLines: [String] {
        notes.map { "\($0.file): " + $0.notes.joined(separator: "; ") }
    }

    package static func photos(_ n: Int) -> String { n == 1 ? "1 photo" : "\(n) photos" }

    static func sample(_ names: [String], limit: Int = 3) -> String {
        let head = names.prefix(limit).joined(separator: ", ")
        return names.count > limit ? "\(head) and \(names.count - limit) more" : head
    }
}

/// What "Revert Lightroom Import" did. Mirrors ds-lr's revert report.
public struct LRRevertSummary: Sendable, Equatable {
    public var restored = 0
    public var kept = 0
    public var variantsRemoved = 0
    public var albumsRemoved = 0
    public var albumsTrimmed = 0
    /// What was reverted, as the message names it.
    public var what = "Lightroom import"

    public init(restored: Int = 0, kept: Int = 0, variantsRemoved: Int = 0, albumsRemoved: Int = 0, albumsTrimmed: Int = 0,
                what: String = "Lightroom import") {
        self.what = what
        self.restored = restored
        self.kept = kept
        self.variantsRemoved = variantsRemoved
        self.albumsRemoved = albumsRemoved
        self.albumsTrimmed = albumsTrimmed
    }

    public var message: String {
        var parts = ["\(what) reverted: \(LRImportSummary.photos(restored)) restored"]
        if variantsRemoved > 0 { parts.append("\(variantsRemoved) imported variant\(variantsRemoved == 1 ? "" : "s") removed") }
        let albums = albumsRemoved + albumsTrimmed
        if albums > 0 { parts.append("\(albums) album\(albums == 1 ? "" : "s") undone") }
        if kept > 0 { parts.append("\(kept) later Darkslide change\(kept == 1 ? "" : "s") kept") }
        return parts.joined(separator: ", ")
    }
}

/// A Lightroom preset mapped to a Darkslide preset.
public struct LRPresetItem: Sendable, Equatable, Identifiable {
    public var id: String { (group ?? "") + "/" + name }
    public var name: String
    public var group: String?
    public var preset: Preset
    /// Approximate and left-out settings, one line each.
    public var notes: [String]
    /// Why it can't be imported (nothing in it maps, or it couldn't be read).
    public var unavailable: String?

    public init(name: String, group: String? = nil, preset: Preset, notes: [String] = [], unavailable: String? = nil) {
        self.name = name
        self.group = group
        self.preset = preset
        self.notes = notes
        self.unavailable = unavailable
    }
}

/// The store side of the import (the live store; demo mode has none).
public protocol LightroomImporting: Sendable {
    /// What the path is (cheap; no full read).
    func lightroomKind(of url: URL) async -> LRSourceKind
    /// Read a catalog or sidecar folder once; later calls refer to it by the returned id.
    func loadLightroomSource(_ url: URL) async throws -> LRSourceInfo
    func releaseLightroomSource(_ id: UUID) async
    func importLightroom(_ session: SessionInfo, source: UUID, categories: Set<LRCategory>, dryRun: Bool,
                         overwriteEdits: Bool) async throws -> LRImportSummary
    func canRevertLightroomImport(_ session: SessionInfo) async -> Bool
    /// Take back the imports since the last revert; `nil`: nothing to revert.
    func revertLightroomImport(_ session: SessionInfo) async throws -> LRRevertSummary?
    func readLightroomPresets(_ url: URL) async throws -> [LRPresetItem]
}

/// The open session's Lightroom state: whether there is an import to revert.
public struct LightroomStatus: Equatable, Sendable {
    public var session: UUID?
    public var canRevert = false
}
