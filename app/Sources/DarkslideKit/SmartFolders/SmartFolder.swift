import Foundation

/// What a smart folder contains (mirrors `ds_session::Filter`). Ratings are
/// 0...5 stars; `minRating == maxRating` is an exact rating, `maxRating == 0`
/// is unrated. Empty/`nil` fields don't constrain.
public struct SmartCriteria: Equatable, Hashable, Sendable {
    public var minRating: Int?
    public var maxRating: Int?
    /// Any of these tags (empty = any).
    public var tags: Set<ColorTag> = []
    public var pick: PickState?
    /// Whitespace-separated terms matched against file name, keywords, camera, lens.
    public var text: String?
    public var camera: String?
    public var lens: String?
    public var dateFrom: Date?
    public var dateTo: Date?
    public var isoMin: Int?
    public var isoMax: Int?
    public var edited: Bool?
    /// All of these keywords (whole keyword, case-insensitive).
    public var keywords: [String] = []

    public init(minRating: Int? = nil, maxRating: Int? = nil, tags: Set<ColorTag> = [], pick: PickState? = nil,
                text: String? = nil, camera: String? = nil, lens: String? = nil, dateFrom: Date? = nil,
                dateTo: Date? = nil, isoMin: Int? = nil, isoMax: Int? = nil, edited: Bool? = nil,
                keywords: [String] = []) {
        self.minRating = minRating
        self.maxRating = maxRating
        self.tags = tags
        self.pick = pick
        self.text = text
        self.camera = camera
        self.lens = lens
        self.dateFrom = dateFrom
        self.dateTo = dateTo
        self.isoMin = isoMin
        self.isoMax = isoMax
        self.edited = edited
        self.keywords = keywords
    }

    public static func exactRating(_ n: Int) -> SmartCriteria { SmartCriteria(minRating: n, maxRating: n) }

    /// Date bounds are calendar times on the camera's clock (L7): "from Aug 6"
    /// means Aug 6 00:00 wherever the photos were taken. `d`'s wall-clock time in
    /// `timeZone` (the user's, where they picked it), as ms read as UTC, which is
    /// how ds-session stores capture times.
    public static func wallClockMs(_ d: Date, timeZone: TimeZone = .current) -> Int64 {
        Int64(((d.timeIntervalSince1970 + Double(timeZone.secondsFromGMT(for: d))) * 1000).rounded())
    }

    /// Inverse of `wallClockMs` (for bounds read back from the store).
    public static func date(wallClockMs ms: Int64, timeZone: TimeZone = .current) -> Date {
        let guess = Date(timeIntervalSince1970: Double(ms) / 1000)
        return guess.addingTimeInterval(-Double(timeZone.secondsFromGMT(for: guess)))
    }

    /// Parts only the session index can answer (keywords aren't on `Photo`).
    public var needsIndex: Bool {
        !keywords.allSatisfy { $0.trimmingCharacters(in: .whitespaces).isEmpty }
            || !(text?.trimmingCharacters(in: .whitespaces).isEmpty ?? true)
    }

    /// Just the index-only parts (sent to the store; the rest is evaluated locally).
    public var indexPart: SmartCriteria { SmartCriteria(text: text, keywords: keywords) }

    /// Local evaluation of everything except keywords/text (see `needsIndex`).
    /// O(1): this runs for every folder on every rating keypress.
    public func matchesLocally(_ p: Photo) -> Bool {
        if let lo = minRating, lo > 0, p.rating < lo { return false }
        if let hi = maxRating, p.rating > hi { return false }
        if !tags.isEmpty, !tags.contains(p.colorTag) { return false }
        if let pick, p.pick != pick { return false }
        if let c = camera?.trimmingCharacters(in: .whitespaces), !c.isEmpty,
           !p.meta.camera.localizedCaseInsensitiveContains(c) { return false }
        if let l = lens?.trimmingCharacters(in: .whitespaces), !l.isEmpty,
           !p.meta.lens.localizedCaseInsensitiveContains(l) { return false }
        if let d = dateFrom, p.meta.wallClockMs < Self.wallClockMs(d) { return false }
        if let d = dateTo, p.meta.wallClockMs > Self.wallClockMs(d) { return false }
        if let i = isoMin, p.meta.iso < i { return false }
        if let i = isoMax, p.meta.iso > i { return false }
        if let e = edited, p.isEdited != e { return false }
        return true
    }
}

/// A smart folder applied as the browser filter: its criteria plus the photos
/// the index matched for keyword/text parts (`nil` = no such parts).
public struct SmartFolderScope: Equatable, Sendable {
    public let id: Int64
    public var criteria: SmartCriteria
    public var indexMatches: Set<PhotoID>?
    /// Built-in key, so a placeholder tag folder can be found again once persisted.
    public var builtin: String?

    public init(id: Int64, criteria: SmartCriteria, indexMatches: Set<PhotoID>? = nil, builtin: String? = nil) {
        self.id = id
        self.criteria = criteria
        self.indexMatches = indexMatches
        self.builtin = builtin
    }

    /// Membership, O(1). Missing originals are never members.
    public func contains(_ p: Photo) -> Bool {
        !p.isMissing && criteria.matchesLocally(p) && (indexMatches?.contains(p.id) ?? true)
    }
}

/// A smart folder: built-in (exact ratings, unrated, picked, rejected, one per
/// tag in use) or the user's own. Membership is evaluated live.
public struct SmartFolder: Identifiable, Equatable, Sendable {
    public let id: Int64
    public var name: String
    /// Stable key of a built-in (`"rating:5"`, `"unrated"`, `"picked"`, `"rejected"`, `"tag:red"`).
    public var builtin: String?
    public var mirror: Bool
    public var criteria: SmartCriteria

    public init(id: Int64, name: String, builtin: String? = nil, mirror: Bool = false, criteria: SmartCriteria) {
        self.id = id
        self.name = name
        self.builtin = builtin
        self.mirror = mirror
        self.criteria = criteria
    }

    public var isBuiltin: Bool { builtin != nil }

    /// The exact rating of a built-in rating folder (`"rating:3"` → 3).
    public var exactRating: Int? {
        guard let b = builtin, b.hasPrefix("rating:") else { return nil }
        return Int(b.dropFirst("rating:".count))
    }

    /// Tag folders are shown only while the tag is in use.
    public var isTagFolder: Bool { builtin?.hasPrefix("tag:") ?? false }

    /// SF Symbol for the row.
    public var symbol: String {
        switch builtin {
        case let b? where b.hasPrefix("rating:"): "star.fill"
        case "unrated": "star.slash"
        case "picked": "flag.fill"
        case "rejected": "xmark.circle"
        case let b? where b.hasPrefix("tag:"): "circle.fill"
        default: "gearshape"
        }
    }

    /// The built-in folders for a set of photos (used by stores without a session
    /// index, e.g. demo mode). Ids are negative so they never collide with ds-session ids.
    public static func builtins(tagsInUse: Set<ColorTag>) -> [SmartFolder] {
        var out: [SmartFolder] = (1...5).reversed().map { n in
            SmartFolder(id: -Int64(n), name: n == 1 ? "1 star" : "\(n) stars", builtin: "rating:\(n)",
                        criteria: .exactRating(n))
        }
        out.append(SmartFolder(id: -10, name: "Unrated", builtin: "unrated", criteria: SmartCriteria(maxRating: 0)))
        out.append(SmartFolder(id: -11, name: "Picked", builtin: "picked", criteria: SmartCriteria(pick: .picked)))
        out.append(SmartFolder(id: -12, name: "Rejected", builtin: "rejected", criteria: SmartCriteria(pick: .rejected)))
        for t in ColorTag.allCases where t != .none && tagsInUse.contains(t) {
            out.append(SmartFolder(id: -20 - Int64(t.rawValue), name: t.name, builtin: "tag:\(t.name.lowercased())",
                                   criteria: SmartCriteria(tags: [t])))
        }
        return out
    }
}
