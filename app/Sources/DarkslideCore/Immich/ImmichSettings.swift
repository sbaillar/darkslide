import DarkslideFFI
import Foundation

/// One album rule (Settings ▸ Immich): the archived photos that match it are kept in
/// an Immich album. Rules apply to every session the archive carries; the album collects
/// the matching photos of all of them.
public struct ImmichAlbumRule: Codable, Equatable, Hashable, Sendable, Identifiable {
    public var id: UUID
    /// The Immich album, e.g. "Darkslide ★★★★★".
    public var album: String
    /// At least this many stars, 0 (any) ... 5.
    public var minRating: Int
    public var pickedOnly: Bool
    /// Only photos with this colour label (nil: any).
    public var label: ColorTag?
    /// Only photos with this keyword ("" : any).
    public var keyword: String

    public init(id: UUID = UUID(), album: String, minRating: Int = 0, pickedOnly: Bool = false, label: ColorTag? = nil,
                keyword: String = "") {
        self.id = id
        self.album = album
        self.minRating = minRating
        self.pickedOnly = pickedOnly
        self.label = label
        self.keyword = keyword
    }

    /// The rule a new setup starts with: the five-star photos.
    public static let fiveStars = ImmichAlbumRule(album: "Darkslide ★★★★★", minRating: 5)

    /// A rule without any criterion would put every photo in the album: it is skipped.
    public var hasCriterion: Bool {
        minRating > 0 || pickedOnly || (label.map { $0 != .none } ?? false)
            || !keyword.trimmingCharacters(in: .whitespaces).isEmpty
    }

    /// "★★★★★ or more · Picked · Red · “beach”".
    public var summary: String {
        var parts: [String] = []
        if minRating > 0 { parts.append(String(repeating: "★", count: min(5, minRating)) + (minRating < 5 ? " or more" : "")) }
        if pickedOnly { parts.append("Picked") }
        if let label, label != .none { parts.append(label.name) }
        let k = keyword.trimmingCharacters(in: .whitespaces)
        if !k.isEmpty { parts.append("“\(k)”") }
        return parts.isEmpty ? "No criterion (skipped)" : parts.joined(separator: " · ")
    }

    /// The FFI rule.
    var ffi: ImmichRule {
        let k = keyword.trimmingCharacters(in: .whitespaces)
        return ImmichRule(album: album.trimmingCharacters(in: .whitespaces), minRating: UInt8(clamping: max(0, minRating)),
                          pick: pickedOnly ? .picked : nil,
                          labels: label.flatMap { $0 == .none ? nil : [RustSessionStore.label($0)] } ?? [],
                          keywords: k.isEmpty ? [] : [k])
    }
}

/// Settings ▸ Immich. The server address, the path mapping, the album rules and the
/// options are preferences; the API key lives only in the Keychain, never in
/// preferences or files. `@unchecked`: UserDefaults is thread-safe; the item is
/// Sendable closures.
public struct ImmichSettings: @unchecked Sendable {
    public static let service = "casa.baillargeon.darkslide.immich"
    static let urlKey = "immichServerURL"
    static let immichRootKey = "immichPathOnServer"
    static let localRootKey = "immichPathOnMac"
    static let sidecarsKey = "immichXMPSidecars"
    static let labelTagsKey = "immichLabelTags"
    static let qualityKey = "immichJPEGQuality"
    static let longEdgeKey = "immichLongEdge"
    static let rulesKey = "immichAlbumRules"
    static let lastSyncKey = "immichLastSync"

    public let defaults: UserDefaults
    public let apiKeyItem: KeychainItem

    public init(defaults: UserDefaults, apiKey: KeychainItem = .keychain(service: service, account: "api-key")) {
        self.defaults = defaults
        apiKeyItem = apiKey
    }

    private func string(_ key: String) -> String {
        (defaults.string(forKey: key) ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func set(_ value: String, _ key: String) {
        let v = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if v.isEmpty { defaults.removeObject(forKey: key) } else { defaults.set(v, forKey: key) }
    }

    /// e.g. `https://immich.example.org` ("" = not set up).
    public var serverURL: String {
        get { string(Self.urlKey) }
        nonmutating set { set(newValue, Self.urlKey) }
    }

    public var apiKey: String? {
        apiKeyItem.read().map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.flatMap { $0.isEmpty ? nil : $0 }
    }

    /// Where Immich sees the archive's folder (e.g. `/mnt/photos`) …
    public var pathOnServer: String {
        get { string(Self.immichRootKey) }
        nonmutating set { set(newValue, Self.immichRootKey) }
    }

    /// … and the same folder on this Mac (e.g. `/Volumes/photos`).
    public var pathOnMac: String {
        get { string(Self.localRootKey) }
        nonmutating set { set(newValue, Self.localRootKey) }
    }

    /// XMP sidecars next to the archived originals (on by default once Immich is set up).
    public var xmpSidecars: Bool {
        get { defaults.object(forKey: Self.sidecarsKey) as? Bool ?? true }
        nonmutating set { defaults.set(newValue, forKey: Self.sidecarsKey) }
    }

    /// Colour label and pick as `Darkslide/…` tags.
    public var labelTags: Bool {
        get { defaults.object(forKey: Self.labelTagsKey) as? Bool ?? true }
        nonmutating set { defaults.set(newValue, forKey: Self.labelTagsKey) }
    }

    /// Developed copies' JPEG quality (the export's full-size default).
    public var quality: Int {
        get { (defaults.object(forKey: Self.qualityKey) as? Int).map { min(100, max(1, $0)) } ?? 92 }
        nonmutating set { defaults.set(min(100, max(1, newValue)), forKey: Self.qualityKey) }
    }

    /// Developed copies' long edge in pixels (0: full size).
    public var longEdge: Int {
        get { max(0, defaults.integer(forKey: Self.longEdgeKey)) }
        nonmutating set { defaults.set(max(0, newValue), forKey: Self.longEdgeKey) }
    }

    public var rules: [ImmichAlbumRule] {
        get {
            guard let data = defaults.data(forKey: Self.rulesKey),
                  let rules = try? JSONDecoder().decode([ImmichAlbumRule].self, from: data) else { return [.fiveStars] }
            return rules
        }
        nonmutating set { defaults.set(try? JSONEncoder().encode(newValue), forKey: Self.rulesKey) }
    }

    /// The last album sync, in words (Settings and the status bar).
    public var lastSync: String? {
        get { defaults.string(forKey: Self.lastSyncKey) }
        nonmutating set { defaults.set(newValue, forKey: Self.lastSyncKey) }
    }

    /// A server and a key: the connection can be tested.
    public var hasServer: Bool { !serverURL.isEmpty && apiKey != nil }

    /// Everything album sync needs.
    public var isSetUp: Bool { hasServer && !pathOnServer.isEmpty && !pathOnMac.isEmpty }

    public var mapping: ImmichPathMapping { ImmichPathMapping(immichRoot: pathOnServer, localRoot: pathOnMac) }

    public var developOptions: ImmichDevelopOptions {
        ImmichDevelopOptions(quality: UInt8(clamping: quality), longEdge: longEdge > 0 ? UInt32(longEdge) : nil,
                             labelTags: labelTags)
    }

    /// What the archive does on each run (nil: Immich isn't set up).
    public var publishConfig: ImmichPublishConfig? {
        isSetUp ? ImmichPublishConfig(xmpSidecars: xmpSidecars, labelTags: labelTags) : nil
    }
}

public enum ImmichError: Error, Equatable, LocalizedError {
    case notSetUp
    case notArchivable(String)
    case failed(String)

    public var errorDescription: String? {
        switch self {
        case .notSetUp: "Set up Immich first in Settings ▸ Immich."
        case .notArchivable(let why): why
        case .failed(let m): m
        }
    }
}

/// What a developed copy looks like on a thumbnail.
public enum ImmichCopyBadge: Sendable, Equatable {
    case current, outOfDate, pending

    init(_ s: ImmichCopyState) {
        switch s {
        case .current: self = .current
        case .outOfDate: self = .outOfDate
        case .pending: self = .pending
        }
    }

    public var help: String {
        switch self {
        case .current: "Developed copy for Immich, up to date"
        case .outOfDate: "Developed copy for Immich, out of date: updated before the next archive run"
        case .pending: "Developed copy for Immich: not made yet"
        }
    }

    public var symbol: String {
        switch self {
        case .current: "photo.badge.checkmark"
        case .outOfDate, .pending: "photo.badge.arrow.down"
        }
    }
}
