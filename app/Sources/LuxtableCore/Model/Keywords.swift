import Foundation

/// Keyword editing (per photo and for the selection). Keywords live in the sidecar
/// (lt-session `set_keywords`) and are written into exports as IPTC keywords and XMP
/// `dc:subject` (lt-export `xmp`; HEIC and hardware JPEG through ImageIO), with the metadata.
public enum Keywords {
    /// "beach, sunset; Crete" → ["beach", "sunset", "Crete"]: split on commas and
    /// semicolons, trimmed, empty dropped, de-duplicated case-insensitively (first spelling kept).
    public static func parse(_ text: String) -> [String] {
        merge([], adding: text.split(whereSeparator: { $0 == "," || $0 == ";" || $0 == "\n" }).map(String.init))
    }

    /// `current` plus `new`, in order, without case-insensitive duplicates.
    public static func merge(_ current: [String], adding new: [String]) -> [String] {
        var out: [String] = []
        var seen = Set<String>()
        for k in current + new {
            let t = k.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !t.isEmpty, seen.insert(t.lowercased()).inserted else { continue }
            out.append(t)
        }
        return out
    }

    /// `current` without `keyword` (case-insensitive).
    public static func removing(_ keyword: String, from current: [String]) -> [String] {
        current.filter { $0.caseInsensitiveCompare(keyword) != .orderedSame }
    }

    /// Keywords every photo in `photos` has (shown as removable for a multi-selection),
    /// in the first photo's order.
    public static func common(_ photos: [Photo]) -> [String] {
        guard let first = photos.first else { return [] }
        return first.keywords.filter { k in
            photos.allSatisfy { p in p.keywords.contains { $0.caseInsensitiveCompare(k) == .orderedSame } }
        }
    }
}

extension AppModel {
    /// Keywords shared by the selection (the focused photo's when one is selected).
    public var selectionKeywords: [String] {
        Keywords.common(selectedIDs.compactMap { photo($0) })
    }

    /// Add comma-separated keywords to every selected photo (one undo step).
    public func addKeywords(_ text: String) {
        let new = Keywords.parse(text)
        guard !new.isEmpty else { return }
        updatePhotos(selectedIDs, label: "Add Keywords") { $0.keywords = Keywords.merge($0.keywords, adding: new) }
    }

    /// Remove a keyword from every selected photo (one undo step).
    public func removeKeyword(_ keyword: String) {
        updatePhotos(selectedIDs, label: "Remove Keyword") { $0.keywords = Keywords.removing(keyword, from: $0.keywords) }
    }
}
