import Foundation

/// A photo's descriptive metadata: the common IPTC Core fields (lt-core `IptcMeta`).
/// Kept in the sidecar (lt-session `set_iptc`), searched by title and caption, and
/// written into exports as XMP/IPTC (the caption also as EXIF `ImageDescription`).
/// `""` is "not set". Editing it never touches the develop settings.
public struct IPTCMetadata: Hashable, Sendable, Codable {
    public var title = ""
    public var caption = ""
    public var creator = ""
    public var copyright = ""
    public var credit = ""
    public var headline = ""
    public var city = ""
    public var state = ""
    public var country = ""
    public var sublocation = ""
    public var instructions = ""
    public var usageTerms = ""

    public init() {}

    /// One field, as the Metadata panel lists them.
    public enum Field: String, CaseIterable, Identifiable, Sendable, Codable {
        case title, caption, headline, creator, credit, copyright
        case sublocation, city, state, country
        case instructions, usageTerms

        public var id: String { rawValue }

        public var label: String {
            switch self {
            case .title: "Title"
            case .caption: "Caption"
            case .headline: "Headline"
            case .creator: "Creator"
            case .credit: "Credit Line"
            case .copyright: "Copyright"
            case .sublocation: "Sublocation"
            case .city: "City"
            case .state: "State / Province"
            case .country: "Country"
            case .instructions: "Instructions"
            case .usageTerms: "Usage Terms"
            }
        }

        /// Long text: edited in a multi-line field.
        public var isLong: Bool { self == .caption || self == .usageTerms || self == .instructions }

        var keyPath: WritableKeyPath<IPTCMetadata, String> {
            switch self {
            case .title: \.title
            case .caption: \.caption
            case .headline: \.headline
            case .creator: \.creator
            case .credit: \.credit
            case .copyright: \.copyright
            case .sublocation: \.sublocation
            case .city: \.city
            case .state: \.state
            case .country: \.country
            case .instructions: \.instructions
            case .usageTerms: \.usageTerms
            }
        }
    }

    public subscript(field: Field) -> String {
        get { self[keyPath: field.keyPath] }
        set { self[keyPath: field.keyPath] = newValue }
    }

    public var isEmpty: Bool { Field.allCases.allSatisfy { self[$0].isEmpty } }

    /// The fields that are set.
    public var setFields: [Field] { Field.allCases.filter { !self[$0].isEmpty } }

    /// `self` with every field `preset` sets; its empty fields leave `self`'s alone.
    public func overlaid(_ preset: IPTCMetadata) -> IPTCMetadata {
        var out = self
        for f in preset.setFields { out[f] = preset[f] }
        return out
    }

    /// Only the credits and rights (creator, credit line, copyright, usage terms): what
    /// "Save as Preset…" keeps, so a preset never stamps one photo's title on others.
    public var credits: IPTCMetadata {
        var out = IPTCMetadata()
        for f in [Field.creator, .credit, .copyright, .usageTerms] { out[f] = self[f] }
        return out
    }

    /// Title or caption contains `text` (the browser's search field).
    public func matches(_ text: String) -> Bool {
        title.localizedCaseInsensitiveContains(text) || caption.localizedCaseInsensitiveContains(text)
    }
}

/// Editing a field for one photo or a whole selection (the Metadata panel).
public enum MetadataEditing {
    /// What a field shows for a selection.
    public enum Shown: Equatable, Sendable {
        /// Every photo has this value (`""`: none set).
        case value(String)
        /// The photos differ: shown as "Mixed".
        case mixed
    }

    public static func shown(_ field: IPTCMetadata.Field, in photos: [Photo]) -> Shown {
        guard let first = photos.first?.metadata[field] else { return .value("") }
        return photos.allSatisfy { $0.metadata[field] == first } ? .value(first) : .mixed
    }

    /// The text a field starts with (a mixed field starts empty, with a "Mixed" prompt).
    public static func initialText(_ shown: Shown) -> String {
        if case .value(let v) = shown { return v }
        return ""
    }

    /// The value to write when the user commits `typed` into a field showing `shown`,
    /// or `nil` when nothing changed: a mixed field left empty keeps every photo's value.
    public static func change(typed: String, shown: Shown) -> String? {
        let t = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        switch shown {
        case .mixed: return t.isEmpty ? nil : t
        case .value(let v): return t == v ? nil : t
        }
    }
}

/// One field being edited: the photos it was opened for and what it showed then.
/// A commit writes to those photos against that baseline, whatever is selected by
/// the time focus leaves the field.
public struct MetadataEdit: Equatable, Sendable {
    public let field: IPTCMetadata.Field
    public let ids: [PhotoID]
    public var baseline: MetadataEditing.Shown
}

extension AppModel {
    /// The photos the Metadata panel edits: the selection (the focused photo when one).
    public var metadataTargets: [Photo] { selectedIDs.compactMap { photo($0) } }

    /// Start editing `field` for the current selection (when the field gains focus).
    public func beginMetadataEdit(_ field: IPTCMetadata.Field) -> MetadataEdit {
        let targets = metadataTargets
        return MetadataEdit(field: field, ids: targets.map(\.id), baseline: MetadataEditing.shown(field, in: targets))
    }

    /// Commit `typed` for `edit`'s photos: only that field is written, only when it
    /// differs from the baseline (one undo step). Returns the edit to continue with
    /// (the field may keep focus after Return): its baseline is now what was written.
    @discardableResult
    public func commitMetadata(_ edit: MetadataEdit, typed: String) -> MetadataEdit {
        guard let value = MetadataEditing.change(typed: typed, shown: edit.baseline) else { return edit }
        let field = edit.field
        updatePhotos(edit.ids, label: "Edit \(field.label)") { $0.metadata[field] = value }
        var next = edit
        next.baseline = .value(value)
        return next
    }

    /// Commit `typed` into `field` for the current selection.
    public func commitMetadata(_ field: IPTCMetadata.Field, typed: String) {
        commitMetadata(beginMetadataEdit(field), typed: typed)
    }

    /// Clear `field` on every selected photo, Mixed or not (one undo step): what an
    /// empty commit into a Mixed field doesn't do.
    public func clearMetadata(_ field: IPTCMetadata.Field) {
        updatePhotos(metadataTargets.map(\.id), label: "Clear \(field.label)") { $0.metadata[field] = "" }
    }

    /// Set every field `preset` sets on the selection (one undo step).
    public func applyMetadataPreset(_ preset: MetadataPreset) {
        updatePhotos(metadataTargets.map(\.id), label: "Apply “\(preset.name)”") {
            $0.metadata = $0.metadata.overlaid(preset.fields)
        }
    }
}
