import Foundation
import Observation

/// The user's keyword list (Capture One's keyword library, flattened): keywords offered
/// in the Keywords panel to add to the selection in one click. Persisted in the app's
/// `UserDefaults` suite (a test suite is injected in tests).
@MainActor @Observable
public final class KeywordLibrary {
    public static let key = "keywordLibrary"
    public static let shared = KeywordLibrary(defaults: AppModel.appDefaults)

    @ObservationIgnored private let defaults: UserDefaults
    public private(set) var keywords: [String]

    public init(defaults: UserDefaults) {
        self.defaults = defaults
        keywords = Keywords.merge([], adding: defaults.stringArray(forKey: Self.key) ?? [])
    }

    public func contains(_ keyword: String) -> Bool {
        keywords.contains { $0.caseInsensitiveCompare(keyword) == .orderedSame }
    }

    /// Add keywords not in the list yet (case-insensitive), in order.
    public func add(_ new: [String]) {
        set(Keywords.merge(keywords, adding: new))
    }

    /// Library keywords not in `current` (case-insensitive), at most `limit`, in order.
    public func suggestions(excluding current: [String], limit: Int = 40) -> [String] {
        let have = Set(current.map { $0.lowercased() })
        return Array(keywords.lazy.filter { !have.contains($0.lowercased()) }.prefix(limit))
    }

    public func remove(_ keyword: String) { remove([keyword]) }

    /// Remove every keyword of `list` (case-insensitive).
    public func remove(_ list: [String]) {
        let gone = Set(list.map { $0.lowercased() })
        set(keywords.filter { !gone.contains($0.lowercased()) })
    }

    private func set(_ list: [String]) {
        guard list != keywords else { return }
        keywords = list
        defaults.set(list, forKey: Self.key)
    }
}
