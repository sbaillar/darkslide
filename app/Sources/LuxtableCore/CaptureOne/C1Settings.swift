import Foundation

// Capture One application settings import (File ▸ Import Capture One Settings…):
// lt-c1 reads the settings folder and maps each item (FFI
// `read_capture_one_settings`); this file plans and applies them to the app's own
// stores, journaled so a revert takes them back. Re-importing is idempotent: an item
// is created once, updated only while it still holds what the import wrote, and
// never written over a change the user made in Luxtable.

/// A setting carried over approximately (`dropped == false`) or not at all.
public struct C1Note: Equatable, Sendable {
    public var key: String
    public var text: String
    public var dropped: Bool

    public init(key: String, text: String, dropped: Bool) {
        self.key = key
        self.text = text
        self.dropped = dropped
    }
}

/// The sheet's checkboxes.
public enum C1SettingsCategory: String, CaseIterable, Codable, Sendable {
    case recipes, workspaces, cropRatios, presets, keywords, preferences

    public var title: String {
        switch self {
        case .recipes: "Export recipes"
        case .workspaces: "Tool workspaces"
        case .cropRatios: "Crop ratios"
        case .presets: "Styles & presets"
        case .keywords: "Keyword libraries"
        case .preferences: "Preferences"
        }
    }

    /// Singular and plural nouns for counts.
    var noun: (String, String) {
        switch self {
        case .recipes: ("recipe", "recipes")
        case .workspaces: ("workspace", "workspaces")
        case .cropRatios: ("crop ratio", "crop ratios")
        case .presets: ("preset", "presets")
        case .keywords: ("keyword library", "keyword libraries")
        case .preferences: ("preference", "preferences")
        }
    }

    func count(_ n: Int) -> String { "\(n) \(n == 1 ? noun.0 : noun.1)" }
}

/// A preference the import carries over.
public enum C1Preference: Equatable, Sendable {
    /// Settings ▸ Import: the rename template (`{original}`…).
    case renameTemplate(String)
    /// The soft-proof profile (also what "Use proof profile" recipes embed).
    case proofProfile(ProofProfile)

    /// The journal's id for it.
    var id: String {
        switch self {
        case .renameTemplate: "renameTemplate"
        case .proofProfile: "proofProfile"
        }
    }
}

/// One thing the import can bring over.
public struct C1SettingsItem: Equatable, Sendable {
    public enum Payload: Equatable, Sendable {
        case recipe(ExportRecipe)
        case workspace(name: String, ToolWorkspace)
        case cropRatio(CustomCropRatio)
        case preset(Preset)
        /// A keyword library: its keywords join the keyword list.
        case keywords([String])
        case preference(C1Preference)
    }

    public var category: C1SettingsCategory
    /// Stable across re-imports: the C1 file (or entry) it came from.
    public var key: String
    public var title: String
    /// Rows are grouped under this (a preset's tool folder).
    public var group: String?
    /// Changes when the C1 file changes (re-imports update only then).
    public var fingerprint: String
    public var notes: [C1Note]
    public var payload: Payload
    /// Why it can't be imported (nothing maps, a built-in already does it).
    public var unavailable: String?

    public init(category: C1SettingsCategory, key: String, title: String, group: String? = nil, fingerprint: String,
                notes: [C1Note] = [], payload: Payload, unavailable: String? = nil) {
        self.category = category
        self.key = key
        self.title = title
        self.group = group
        self.fingerprint = fingerprint
        self.notes = notes
        self.payload = payload
        self.unavailable = unavailable
    }

    /// The name a new Luxtable item gets (and is checked against for clashes).
    var name: String {
        switch payload {
        case .recipe(let r): r.name
        case .workspace(let name, _): name
        case .cropRatio(let r): r.name
        case .preset(let p): p.name
        case .keywords: title
        case .preference(let p): p.id
        }
    }
}

/// What a Capture One settings folder holds, mapped (see lt-c1 `settings`).
public struct C1SettingsScan: Equatable, Sendable {
    public struct Folder: Equatable, Sendable {
        public var name: String
        public var files: Int

        public init(name: String, files: Int) {
            self.name = name
            self.files = files
        }
    }

    public struct Skipped: Equatable, Sendable {
        public var file: String
        public var reason: String

        public init(file: String, reason: String) {
            self.file = file
            self.reason = reason
        }
    }

    public var folder: URL
    public var items: [C1SettingsItem]
    /// Settings found but not carried over, not tied to one item (preferences, workspace windows).
    public var notes: [C1Note]
    public var folders: [Folder]
    public var skipped: [Skipped]

    public init(folder: URL, items: [C1SettingsItem] = [], notes: [C1Note] = [], folders: [Folder] = [],
                skipped: [Skipped] = []) {
        self.folder = folder
        self.items = items
        self.notes = notes
        self.folders = folders
        self.skipped = skipped
    }

    public func items(in c: C1SettingsCategory) -> [C1SettingsItem] { items.filter { $0.category == c } }
}

/// Reads a Capture One settings folder (seam: lt-c1 through the FFI live, a fake in tests).
public protocol C1SettingsReading: Sendable {
    func read(folder: URL, preferences: URL?) async throws -> C1SettingsScan
}

// MARK: - Planning

/// What the import does (or did) with one item.
public enum C1SettingsAction: Equatable, Sendable {
    /// Not in Luxtable yet: created.
    case create
    /// Imported before and unchanged in Luxtable since, but changed in Capture One: updated.
    case update(id: String)
    /// Imported before; nothing changed in either app.
    case upToDate
    /// Changed in Luxtable since the import: left alone.
    case keptYours
    /// Deleted in Luxtable since the import: not recreated.
    case deletedInLuxtable
    /// Luxtable already has one by that name (or the same value): left alone.
    case alreadyInLuxtable
    /// Nothing about it can be imported.
    case unavailable(String)

    /// The import writes something.
    public var writes: Bool {
        switch self {
        case .create, .update: true
        default: false
        }
    }

    public var label: String {
        switch self {
        case .create: "new"
        case .update: "updated"
        case .upToDate: "up to date"
        case .keptYours: "changed in Luxtable, kept"
        case .deletedInLuxtable: "deleted in Luxtable, not recreated"
        case .alreadyInLuxtable: "already in Luxtable"
        case .unavailable(let why): why
        }
    }
}

/// The import's record of what it wrote (`<support>/capture-one-settings-import.json`).
/// Re-imports fold into it (the first "before" stays); a revert consumes it.
public struct C1SettingsJournal: Codable, Equatable, Sendable {
    public struct Entry: Codable, Equatable, Sendable {
        public var category: C1SettingsCategory
        public var key: String
        /// The Luxtable item's id (recipe/preset/ratio UUID, workspace name, the keywords a
        /// library added, preference).
        public var id: String
        public var fingerprint: String
        /// What the import left there (compared to tell whether the user changed it since).
        public var written: Data
        /// A preference's value before the first import (nil: the import created the item).
        public var before: Data?
    }

    public static let fileName = "capture-one-settings-import.json"
    public var version = 1
    public var entries: [Entry] = []

    public init() {}

    func entry(_ c: C1SettingsCategory, _ key: String) -> Entry? {
        entries.first { $0.category == c && $0.key == key }
    }

    mutating func record(_ e: Entry) {
        if let i = entries.firstIndex(where: { $0.category == e.category && $0.key == e.key }) {
            var merged = e
            merged.before = entries[i].before
            entries[i] = merged
        } else {
            entries.append(e)
        }
    }

    /// Unreadable or missing: an empty journal (nothing to revert; items then count as
    /// the user's, so nothing is overwritten). A journal that exists but doesn't decode is
    /// moved aside to `<name>.bak` first, so the next import's save doesn't destroy the only
    /// record of what an earlier import changed.
    public static func load(_ url: URL) -> C1SettingsJournal {
        guard let data = try? Data(contentsOf: url) else { return C1SettingsJournal() }
        if let journal = try? JSONDecoder().decode(Self.self, from: data) { return journal }
        let backup = url.appendingPathExtension("bak")
        try? FileManager.default.removeItem(at: backup)
        try? FileManager.default.moveItem(at: url, to: backup)
        return C1SettingsJournal()
    }

    /// Atomic write; an empty journal removes the file.
    public func save(_ url: URL) throws {
        if entries.isEmpty {
            if FileManager.default.fileExists(atPath: url.path) { try FileManager.default.removeItem(at: url) }
            return
        }
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let enc = JSONEncoder()
        enc.outputFormatting = [.sortedKeys, .prettyPrinted]
        try enc.encode(self).write(to: url, options: .atomic)
    }
}

/// Where imported items live (seam: the app's stores live, fakes in tests). Each item
/// is compared as a snapshot (`Data`), so "changed since the import" is exact.
@MainActor
public protocol C1SettingsTargets: AnyObject {
    /// The Luxtable item `id` as stored now; nil when it no longer exists.
    func snapshot(_ category: C1SettingsCategory, id: String) -> Data?
    /// Luxtable already has `item` (by name, or a preference already at its value).
    func exists(_ item: C1SettingsItem) -> Bool
    /// Create the item; returns its id.
    func create(_ item: C1SettingsItem) throws -> String
    /// Update item `id`; returns its id afterwards (a keyword library's grows).
    func replace(_ id: String, with item: C1SettingsItem) throws -> String
    func remove(_ category: C1SettingsCategory, id: String)
    /// Put a preference back to `data` (a snapshot taken before the import).
    func restore(_ category: C1SettingsCategory, id: String, to data: Data)
    /// The current value of a preference, before the import changes it.
    func before(_ item: C1SettingsItem) -> Data?
}

/// What an import (or its dry run) does per item.
public struct C1SettingsPlan: Equatable, Sendable {
    public struct Row: Equatable, Sendable {
        public var item: C1SettingsItem
        public var action: C1SettingsAction
    }

    public var rows: [Row]

    public func rows(in c: C1SettingsCategory) -> [Row] { rows.filter { $0.item.category == c } }

    /// Items the import writes, in `categories`.
    public func changes(in categories: Set<C1SettingsCategory>) -> Int {
        rows.count { categories.contains($0.item.category) && $0.action.writes }
    }

    /// "3 new, 1 updated, 2 up to date" for a category.
    public func summary(_ c: C1SettingsCategory) -> String {
        let r = rows(in: c)
        guard !r.isEmpty else { return "none found" }
        var counts: [(String, Int)] = []
        for row in r {
            let label = if case .unavailable = row.action { "not importable" } else { row.action.label }
            if let i = counts.firstIndex(where: { $0.0 == label }) { counts[i].1 += 1 } else { counts.append((label, 1)) }
        }
        return counts.map { "\($0.1) \($0.0)" }.joined(separator: ", ")
    }
}

public enum C1SettingsError: LocalizedError, Equatable {
    /// An item was written but couldn't be read back.
    case notSaved(String)

    public var errorDescription: String? {
        switch self {
        case .notSaved(let what): "\u{201c}\(what)\u{201d} couldn't be saved"
        }
    }
}

/// What a revert did.
public struct C1SettingsRevert: Equatable, Sendable {
    public var removed = 0
    public var restored = 0
    /// Changed in Luxtable since the import: left alone.
    public var kept = 0

    public var message: String {
        var parts = ["Capture One settings import reverted: \(removed) item\(removed == 1 ? "" : "s") removed"]
        if restored > 0 { parts.append("\(restored) preference\(restored == 1 ? "" : "s") restored") }
        if kept > 0 { parts.append("\(kept) changed since and kept") }
        return parts.joined(separator: ", ")
    }
}

/// Plans, applies and reverts against `targets`, journaled at `journalURL`.
@MainActor
public struct C1SettingsImporter {
    public let targets: any C1SettingsTargets
    public let journalURL: URL

    public init(targets: any C1SettingsTargets, journalURL: URL) {
        self.targets = targets
        self.journalURL = journalURL
    }

    public var journal: C1SettingsJournal { .load(journalURL) }

    /// An import wrote something that a revert can take back.
    public var canRevert: Bool { !journal.entries.isEmpty }

    public func plan(_ scan: C1SettingsScan) -> C1SettingsPlan {
        let j = journal
        return C1SettingsPlan(rows: scan.items.map { .init(item: $0, action: action(for: $0, entry: j.entry($0.category, $0.key))) })
    }

    func action(for item: C1SettingsItem, entry: C1SettingsJournal.Entry?) -> C1SettingsAction {
        if let why = item.unavailable { return .unavailable(why) }
        guard let entry else { return targets.exists(item) ? .alreadyInLuxtable : .create }
        guard let current = targets.snapshot(item.category, id: entry.id) else { return .deletedInLuxtable }
        if current != entry.written { return .keptYours }
        if entry.fingerprint == item.fingerprint { return .upToDate }
        return targets.exists(item) && item.category == .preferences ? .upToDate : .update(id: entry.id)
    }

    /// Apply the plan's writes in `categories`. The journal is saved after each one, so
    /// an import that stops part-way can still be reverted; an item whose journal entry
    /// can't be saved is taken out again, so nothing is left that a revert can't reach.
    /// Returns the plan as done.
    @discardableResult
    public func apply(_ plan: C1SettingsPlan, categories: Set<C1SettingsCategory>) throws -> C1SettingsPlan {
        var j = journal
        for row in plan.rows where categories.contains(row.item.category) && row.action.writes {
            let item = row.item
            let before = item.category == .preferences ? targets.before(item) : nil
            let id: String
            var created = false
            if case .update(let existing) = row.action {
                id = try targets.replace(existing, with: item)
            } else {
                id = try targets.create(item)
                created = true
            }
            do {
                guard let written = targets.snapshot(item.category, id: id) else {
                    throw C1SettingsError.notSaved(item.title)
                }
                var next = j
                next.record(.init(category: item.category, key: item.key, id: id, fingerprint: item.fingerprint,
                                  written: written, before: before))
                try next.save(journalURL)
                j = next
            } catch {
                if created {
                    if let before { targets.restore(item.category, id: id, to: before) } else { targets.remove(item.category, id: id) }
                }
                throw error
            }
        }
        return plan
    }

    /// Take back every import since the last revert: items still as the import left
    /// them are removed (preferences put back); ones changed since are kept.
    public func revert() throws -> C1SettingsRevert {
        let j = journal
        var r = C1SettingsRevert()
        for e in j.entries.reversed() {
            guard let current = targets.snapshot(e.category, id: e.id) else { continue }
            guard current == e.written else {
                r.kept += 1
                continue
            }
            if let before = e.before {
                targets.restore(e.category, id: e.id, to: before)
                r.restored += 1
            } else {
                targets.remove(e.category, id: e.id)
                r.removed += 1
            }
        }
        try C1SettingsJournal().save(journalURL)
        return r
    }
}
