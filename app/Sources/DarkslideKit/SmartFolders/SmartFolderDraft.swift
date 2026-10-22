import Foundation

/// What the smart-folder editor sheet is for.
public enum SmartFolderEditorTarget: Identifiable, Equatable, Sendable {
    case new
    case edit(SmartFolder)

    public var id: String {
        switch self {
        case .new: "new"
        case .edit(let f): "edit-\(f.id)"
        }
    }

    public var draft: SmartFolderDraft {
        switch self {
        case .new: SmartFolderDraft()
        case .edit(let f): SmartFolderDraft(f)
        }
    }
}

/// The "New Smart Folder…" / "Edit Smart Folder…" form state (kept out of the
/// view so the round trip is unit-tested).
public struct SmartFolderDraft: Equatable, Sendable {
    public enum RatingMode: String, CaseIterable, Sendable {
        case any = "Any", exactly = "Exactly", atLeast = "At least", atMost = "At most", between = "Between", unrated = "Unrated"
    }

    public enum EditedMode: String, CaseIterable, Sendable {
        case any = "Any", edited = "Edited", unedited = "Not edited"
    }

    public var name = ""
    public var ratingMode = RatingMode.any
    /// The rating for exactly/at least/at most, and the low end of between.
    public var rating = 3
    public var ratingHigh = 5
    public var tags: Set<ColorTag> = []
    public var pick: PickState?
    public var camera = ""
    public var lens = ""
    public var text = ""
    public var useDateFrom = false
    public var dateFrom = Date()
    public var useDateTo = false
    public var dateTo = Date()
    public var isoMin = ""
    public var isoMax = ""
    /// Comma-separated; all must match.
    public var keywords = ""
    public var edited = EditedMode.any

    public init() {}

    public init(_ f: SmartFolder) {
        name = f.name
        let c = f.criteria
        switch (c.minRating.flatMap { $0 > 0 ? $0 : nil }, c.maxRating) {
        case (nil, nil): ratingMode = .any
        case (nil, 0?): ratingMode = .unrated
        case (let lo?, let hi?) where lo == hi: ratingMode = .exactly; rating = lo
        case (let lo?, nil): ratingMode = .atLeast; rating = lo
        case (nil, let hi?): ratingMode = .atMost; rating = hi
        case (let lo?, let hi?): ratingMode = .between; rating = lo; ratingHigh = hi
        }
        tags = c.tags
        pick = c.pick
        camera = c.camera ?? ""
        lens = c.lens ?? ""
        text = c.text ?? ""
        if let d = c.dateFrom { useDateFrom = true; dateFrom = d }
        if let d = c.dateTo { useDateTo = true; dateTo = d }
        isoMin = c.isoMin.map(String.init) ?? ""
        isoMax = c.isoMax.map(String.init) ?? ""
        keywords = c.keywords.joined(separator: ", ")
        edited = switch c.edited {
        case true?: .edited
        case false?: .unedited
        case nil: .any
        }
    }

    public var criteria: SmartCriteria {
        var c = SmartCriteria()
        switch ratingMode {
        case .any: break
        case .exactly: c.minRating = rating; c.maxRating = rating
        case .atLeast: c.minRating = rating
        case .atMost: c.maxRating = rating
        case .between: c.minRating = min(rating, ratingHigh); c.maxRating = max(rating, ratingHigh)
        case .unrated: c.maxRating = 0
        }
        c.tags = tags
        c.pick = pick
        c.camera = Self.nonEmpty(camera)
        c.lens = Self.nonEmpty(lens)
        c.text = Self.nonEmpty(text)
        // Whole days: from the start of the first to the end of the last (L7).
        let cal = Calendar.current
        c.dateFrom = useDateFrom ? cal.startOfDay(for: dateFrom) : nil
        c.dateTo = useDateTo ? cal.date(byAdding: .day, value: 1, to: cal.startOfDay(for: dateTo))?.addingTimeInterval(-0.001) : nil
        c.isoMin = Int(isoMin.trimmingCharacters(in: .whitespaces))
        c.isoMax = Int(isoMax.trimmingCharacters(in: .whitespaces))
        c.keywords = keywords.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        c.edited = switch edited {
        case .any: nil
        case .edited: true
        case .unedited: false
        }
        return c
    }

    /// Saving needs a name (the store also rejects blank ones).
    public var canSave: Bool { !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }

    private static func nonEmpty(_ s: String) -> String? {
        let t = s.trimmingCharacters(in: .whitespaces)
        return t.isEmpty ? nil : t
    }
}
