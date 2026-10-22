import Foundation

/// Left-hand tool tabs (Capture One layout). Each tab is a stack of panels.
/// The built-in tabs are static members; users add custom tabs (`custom-…` ids),
/// whose name, symbol and panels live in `ToolWorkspace`.
public struct ToolTab: RawRepresentable, Hashable, Identifiable, Codable, Sendable, CaseIterable {
    public let rawValue: String

    /// Built-in ids and `custom-…` ids; anything else (a tab removed since) is nil.
    public init?(rawValue: String) {
        guard Self.builtIns.contains(where: { $0.rawValue == rawValue })
            || (rawValue.hasPrefix(Self.customPrefix) && rawValue.count > Self.customPrefix.count) else { return nil }
        self.rawValue = rawValue
    }

    private init(_ raw: String) { rawValue = raw }

    public static let favorites = ToolTab("favorites")
    public static let library = ToolTab("library")
    public static let importArchive = ToolTab("importArchive")
    public static let color = ToolTab("color")
    public static let exposure = ToolTab("exposure")
    public static let lens = ToolTab("lens")
    public static let composition = ToolTab("composition")
    public static let details = ToolTab("details")
    public static let layers = ToolTab("layers")
    public static let metadata = ToolTab("metadata")
    public static let output = ToolTab("output")
    /// Every tool in one stack. Hidden by default; Tools ▸ Show All Tools Tab adds it.
    public static let allTools = ToolTab("allTools")

    /// The default tab strip, in order.
    public static let allCases: [ToolTab] = [
        .favorites, .library, .importArchive, .color, .exposure, .lens, .composition, .details, .layers, .metadata, .output,
    ]
    /// Every built-in tab: the default strip plus the hidden-by-default ones.
    public static let builtIns: [ToolTab] = allCases + [.allTools]

    static let customPrefix = "custom-"

    /// A new user tab.
    public static func newCustom() -> ToolTab { ToolTab(customPrefix + UUID().uuidString.lowercased()) }

    public var isCustom: Bool { rawValue.hasPrefix(Self.customPrefix) }

    public var id: String { rawValue }

    /// Built-in name (custom tabs are named in the workspace).
    public var title: String {
        switch rawValue {
        case "favorites": "Favorites"
        case "library": "Library"
        case "importArchive": "Import & Archive"
        case "color": "Color"
        case "exposure": "Exposure"
        case "lens": "Lens"
        case "composition": "Composition"
        case "details": "Details"
        case "layers": "Layers"
        case "metadata": "Metadata"
        case "output": "Output"
        case "allTools": "All Tools"
        default: "Custom"
        }
    }

    public var symbol: String {
        switch rawValue {
        case "favorites": "star"
        case "library": "photo.stack"
        case "importArchive": "sdcard"
        case "color": "paintpalette"
        case "exposure": "sun.max"
        case "lens": "camera.aperture"
        case "composition": "crop.rotate"
        case "details": "triangle"
        case "layers": "square.3.layers.3d"
        case "metadata": "info.circle"
        case "output": "square.and.arrow.up"
        case "allTools": "list.bullet.rectangle"
        default: "square.grid.2x2"
        }
    }

    /// Built-in panels (the default workspace). For `.favorites` this is the
    /// starter set; custom tabs start empty.
    public var panels: [ToolPanelID] {
        switch rawValue {
        case "favorites": [.whiteBalance, .exposure, .styles, .history]
        case "library": [.smartFolders, .sessions, .albums, .filters, .people]
        case "importArchive": [.importCard, .archive]
        // N12: White Balance first (the most-used tool); Styles & Presets and History last,
        // collapsed by default.
        case "color": [.whiteBalance, .colorBalance, .colorEditor, .styles, .history]
        case "exposure": [.exposure, .presence, .hdr, .levels, .curve]
        case "lens": [.lensCorrection, .vignette]
        case "composition": [.crop]
        case "details": [.sharpening, .noise, .clarity, .spots, .effects]
        case "layers": [.layers]
        case "metadata": [.metadata, .keywords]
        case "output": [.export, .amazonPhotos]
        case "allTools": Self.homeTabs.flatMap(\.panels)
        default: []
        }
    }

    /// The tabs that own each tool by default (what the Tools menu groups by).
    public static let homeTabs: [ToolTab] = allCases.filter { $0 != .favorites }

    /// Symbols offered for custom tabs.
    public static let customSymbols = [
        "square.grid.2x2", "star", "heart", "bolt", "flag", "wand.and.stars", "paintbrush", "slider.horizontal.3",
        "camera", "person.crop.square", "leaf", "moon", "sparkles", "circle.lefthalf.filled",
    ]
}

/// Every collapsible tool panel.
public enum ToolPanelID: String, CaseIterable, Identifiable, Sendable {
    case smartFolders, sessions, albums, filters, importCard, archive
    case styles, whiteBalance, colorBalance, colorEditor
    case exposure, hdr, levels, curve
    case lensCorrection, vignette, effects, crop
    case sharpening, noise, clarity, layers
    case presence, spots
    case metadata, keywords, export, amazonPhotos
    /// [find] People: faces, names, "Who is this?".
    case people
    /// [history] The focused photo's edit steps (jump back, hover to preview).
    case history

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .smartFolders: "Smart Folders"
        case .sessions: "Sessions"
        case .albums: "Albums"
        case .filters: "Filters"
        case .importCard: "Memory Card"
        case .archive: "Archive"
        case .styles: "Styles & Presets"
        case .whiteBalance: "White Balance"
        case .colorBalance: "Color Balance"
        case .colorEditor: "Color Editor"
        case .exposure: "Exposure"
        case .hdr: "High Dynamic Range"
        case .levels: "Levels"
        case .curve: "Curve"
        case .lensCorrection: "Lens Correction"
        case .vignette: "Vignetting"
        case .effects: "Effects"
        case .crop: "Crop & Straighten"
        case .sharpening: "Sharpening"
        case .noise: "Noise Reduction"
        case .clarity: "Clarity"
        case .layers: "Layers"
        case .presence: "Presence"
        case .spots: "Heal & Clone"
        case .metadata: "Metadata"
        case .keywords: "Keywords"
        case .export: "Export"
        case .amazonPhotos: "Amazon Photos"
        case .people: "People"
        case .history: "History"
        }
    }

    /// The built-in tab that holds this tool by default (where a re-added tool goes).
    public var homeTab: ToolTab {
        ToolTab.homeTabs.first { $0.panels.contains(self) } ?? .favorites
    }

    /// The copy/apply group this panel edits (drives per-panel reset and copy).
    public var group: AdjustGroup? {
        switch self {
        case .whiteBalance: .whiteBalance
        case .colorBalance: .colorBalance
        case .colorEditor: .colorEditor
        case .exposure: .exposure
        case .hdr: .hdr
        case .levels: .levels
        case .curve: .curve
        case .lensCorrection: .lens
        case .vignette: .vignette
        case .effects: .effects
        case .crop: .crop
        case .sharpening, .clarity: .detail
        case .noise: .noise
        case .layers: .layers
        case .presence: .presence
        case .spots: .spots
        default: nil
        }
    }
}

public enum BrowserMode: String, Sendable {
    /// Viewer with a filmstrip below.
    case filmstrip
    /// Full grid in place of the viewer.
    case grid
    /// [map] Map of where photos were taken, with the filmstrip below.
    case map
    /// [library] Every session's photos in one grid (no session needed).
    case library
}

public struct PhotoFilter: Equatable, Sendable {
    public enum PickFilter: String, CaseIterable, Sendable {
        case all = "All", picked = "Picked", unflagged = "Unflagged", unrejected = "Not Rejected", rejected = "Rejected"
    }

    public var minRating = 0
    /// Only photos with no stars (the "still to cull" view).
    public var unratedOnly = false
    /// Show only photos whose original is missing (Remove / Locate). Otherwise missing ones are hidden.
    public var missingOnly = false
    public var colorTag: ColorTag?
    public var pick: PickFilter = .all
    public var text = ""
    /// Show only this smart folder's members (Library ▸ Smart Folders, ⌘1…⌘5).
    public var smartFolder: SmartFolderScope?
    /// [find] Only photos this person is tagged on (People ▸ a person); any case.
    public var person: String?
    /// [map] Only the photos of a marker clicked on the map, or of an area.
    public var place: PlaceFilter?

    public init(minRating: Int = 0, colorTag: ColorTag? = nil, pick: PickFilter = .all, text: String = "",
                unratedOnly: Bool = false, smartFolder: SmartFolderScope? = nil) {
        self.minRating = minRating
        self.unratedOnly = unratedOnly
        self.colorTag = colorTag
        self.pick = pick
        self.text = text
        self.smartFolder = smartFolder
    }

    public var isActive: Bool { self != PhotoFilter() }

    /// The filter bar's rating/tag/flag criteria (what a smart folder replaces).
    private func barCriteriaChanged(_ o: PhotoFilter) -> Bool {
        minRating != o.minRating || unratedOnly != o.unratedOnly || colorTag != o.colorTag || pick != o.pick
    }

    /// A smart folder and the filter bar's rating/tag/flag criteria never stack
    /// (L8): selecting a folder resets them (`AppModel.selectSmartFolder`), and
    /// changing them while a folder is shown leaves the folder. Search text and
    /// the Missing view combine with either.
    public func reconciled(from old: PhotoFilter) -> PhotoFilter {
        guard smartFolder != nil, smartFolder?.id == old.smartFolder?.id, barCriteriaChanged(old) else { return self }
        var f = self
        f.smartFolder = nil
        return f
    }

    public func matches(_ p: Photo) -> Bool {
        if p.inTrash { return false } // [cul9] H7: moved to the session Trash (recoverable)
        if p.isMissing != missingOnly { return false }
        if p.rating < minRating { return false }
        if unratedOnly, p.rating > 0 { return false }
        if let colorTag, p.colorTag != colorTag { return false }
        switch pick {
        case .all: break
        case .picked: if p.pick != .picked { return false }
        case .unflagged: if p.pick != .none { return false }
        case .unrejected: if p.pick == .rejected { return false }
        case .rejected: if p.pick != .rejected { return false }
        }
        if !text.isEmpty, !p.filename.localizedCaseInsensitiveContains(text), !p.metadata.matches(text) { return false }
        if let smartFolder, !smartFolder.contains(p) { return false }
        if let person, !p.people.contains(where: { $0.caseInsensitiveCompare(person) == .orderedSame }) { return false }
        if let place, !place.matches(p) { return false }
        return true
    }
}

/// Browser sort order.
public enum PhotoSort: String, CaseIterable, Sendable {
    case captureTime = "Capture Time", filename = "Filename", rating = "Rating"
}

public struct Album: Identifiable, Hashable, Sendable {
    public let id: UUID
    public var name: String
    public var photoIDs: [PhotoID]

    public init(id: UUID = UUID(), name: String, photoIDs: [PhotoID]) {
        self.id = id
        self.name = name
        self.photoIDs = photoIDs
    }
}
