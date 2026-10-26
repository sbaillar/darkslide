import Foundation

/// Filter-bar interactions (pure; the bar view only calls these).
extension PhotoFilter {
    /// Clicking the active star threshold again clears it (Lightroom behaviour).
    /// Choosing a threshold turns "Unrated" off (they contradict).
    public mutating func toggleMinRating(_ stars: Int) {
        let n = min(max(stars, 0), 5)
        minRating = minRating == n ? 0 : n
        if minRating > 0 { unratedOnly = false }
    }

    /// "Unrated" turns a star threshold off (they contradict).
    public mutating func toggleUnrated() {
        unratedOnly.toggle()
        if unratedOnly { minRating = 0 }
    }

    /// Clicking the selected tag chip clears the tag filter.
    public mutating func toggleTag(_ tag: ColorTag) {
        colorTag = colorTag == tag ? nil : tag
    }

    /// Number of independent criteria in effect (for the "Clear (n)" button).
    public var activeCriteria: Int {
        var n = 0
        if minRating > 0 { n += 1 }
        if unratedOnly { n += 1 }
        if colorTag != nil { n += 1 }
        if pick != .all { n += 1 }
        if !text.trimmingCharacters(in: .whitespaces).isEmpty { n += 1 }
        if place != nil { n += 1 }
        return n
    }

    /// Pick states offered by the bar's segmented control, in order.
    public static let barPickOptions: [PickFilter] = [.all, .picked, .unflagged, .rejected]
}

public enum FilterSummary {
    /// "2,000 photos" when unfiltered, "37 of 2,000" when filtered.
    public static func text(shown: Int, total: Int, filtered: Bool) -> String {
        filtered ? "\(shown.formatted()) of \(total.formatted())"
            : "\(total.formatted()) photo\(total == 1 ? "" : "s")"
    }
}

extension PhotoSort {
    /// Stable sort of `photos` by this order (capture time and filename ascending,
    /// rating descending; ties keep the incoming order).
    public func apply(to photos: [Photo]) -> [Photo] {
        let indexed = photos.enumerated().map { ($0.offset, $0.element) }
        let sorted = indexed.sorted { a, b in
            let (ia, pa) = a, (ib, pb) = b
            switch self {
            case .captureTime:
                if pa.meta.captureDate != pb.meta.captureDate { return pa.meta.captureDate < pb.meta.captureDate }
            case .filename:
                let c = pa.filename.localizedStandardCompare(pb.filename)
                if c != .orderedSame { return c == .orderedAscending }
            case .rating:
                if pa.rating != pb.rating { return pa.rating > pb.rating }
            }
            return ia < ib
        }
        return sorted.map(\.1)
    }

    public var symbol: String {
        switch self {
        case .captureTime: "clock"
        case .filename: "textformat"
        case .rating: "star"
        }
    }
}

/// VoiceOver text for a browser cell and the viewer image.
public enum PhotoAccessibility {
    public static func label(_ p: Photo, stackCount: Int? = nil) -> String {
        var parts = [p.filename]
        parts.append(p.rating == 0 ? "unrated" : "\(p.rating) star\(p.rating == 1 ? "" : "s")")
        switch p.pick {
        case .picked: parts.append("picked")
        case .rejected: parts.append("rejected")
        case .none: break
        }
        if p.colorTag != .none { parts.append("\(p.colorTag.name.lowercased()) tag") }
        if p.isEdited { parts.append("adjusted") }
        if let n = stackCount { parts.append("stack of \(n) photos") }
        if let b = MissingActions.badge(p) { parts.append(b.lowercased()) }
        return parts.joined(separator: ", ")
    }

    public static func isSelected(_ selected: Bool, _ focused: Bool) -> Bool { selected || focused }

    /// The rating after a VoiceOver increment / decrement on a cell, kept within 0...5.
    public static func adjustedRating(_ current: Int, increment: Bool) -> Int {
        min(max(current + (increment ? 1 : -1), 0), 5)
    }

    /// VoiceOver name of the viewer's image (its `.isImage` trait says "image"; zoom is its value).
    public static func viewerLabel(_ p: Photo) -> String { label(p) }
}
