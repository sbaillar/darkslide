import Foundation
import Observation

/// Built-in styles plus the user's presets.
///
/// Persistence is a JSON file (atomic write) at an injected URL, in the app's
/// support folder (shared by every session on this Mac).
@MainActor @Observable
public final class PresetStore {
    public static let shared = PresetStore(fileURL: PresetStore.defaultURL)

    public static var defaultURL: URL? { AppPaths.support.appendingPathComponent("presets.json") }

    public private(set) var userPresets: [Preset] = []
    /// The last load/save error, shown in the panel.
    public private(set) var lastError: String?
    @ObservationIgnored private let fileURL: URL?

    public init(fileURL: URL?) {
        self.fileURL = fileURL
        load()
    }

    public var all: [Preset] { Preset.builtIns + userPresets }

    /// The preset applied to new imports, if any.
    public var autoApplyPreset: Preset? { userPresets.first(where: \.autoApplyOnImport) }

    /// Save `edit` restricted to `groups` as a new user preset. `relative: nil` uses the
    /// non-destructive default (exposure and HDR add to the photo's own values).
    @discardableResult
    public func save(name: String, edit: EditState, groups: Set<AdjustGroup>, relative: Set<AdjustGroup>? = nil) -> Preset? {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        let groups = groups.subtracting([.layers])
        guard !trimmed.isEmpty, !groups.isEmpty else {
            lastError = trimmed.isEmpty ? "Give the preset a name." : "Choose at least one adjustment group."
            return nil
        }
        let p = Preset(name: uniqueName(trimmed), groups: groups, relative: relative ?? Preset.defaultRelative(for: groups),
                       edit: edit)
        userPresets.append(p)
        persist()
        return p
    }

    public func delete(_ id: UUID) {
        userPresets.removeAll { $0.id == id }
        persist()
    }

    public func rename(_ id: UUID, to name: String) {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty, let i = userPresets.firstIndex(where: { $0.id == id }) else { return }
        guard userPresets[i].name != trimmed else { return }
        userPresets[i].name = uniqueName(trimmed)
        persist()
    }

    /// Replace a user preset's settings with `edit` (same name, groups and modes).
    public func update(_ id: UUID, from edit: EditState) {
        guard let i = userPresets.firstIndex(where: { $0.id == id }) else { return }
        userPresets[i].edit = edit
        persist()
    }

    /// Add `preset` to My Presets as it is (its name made unique).
    @discardableResult
    public func add(_ preset: Preset) -> Preset {
        var p = preset
        p.name = uniqueName(preset.name)
        userPresets.append(p)
        persist()
        return p
    }

    /// Replace user preset `id`'s name, groups and settings with `preset`'s (its id and
    /// auto-apply choice stay).
    public func replace(_ id: UUID, with preset: Preset) {
        guard let i = userPresets.firstIndex(where: { $0.id == id }) else { return }
        var p = preset
        p.id = id
        p.name = preset.name == userPresets[i].name ? preset.name : uniqueName(preset.name)
        p.autoApplyOnImport = userPresets[i].autoApplyOnImport
        userPresets[i] = p
        persist()
    }

    /// Copy any preset (a built-in style too) into My Presets, to adjust it.
    @discardableResult
    public func duplicate(_ preset: Preset) -> Preset {
        let p = Preset(name: uniqueName(preset.name + " Copy"), groups: preset.groups, relative: preset.relative,
                       absoluteFields: preset.absoluteFields, edit: preset.edit)
        userPresets.append(p)
        persist()
        return p
    }

    /// Built-in styles of `category`, in pack order.
    public func styles(in category: String) -> [Preset] {
        Preset.builtIns.filter { $0.category == category }
    }

    /// Make `id` the auto-apply preset (or clear with `nil`).
    public func setAutoApply(_ id: UUID?) {
        for i in userPresets.indices { userPresets[i].autoApplyOnImport = userPresets[i].id == id }
        persist()
    }

    private func uniqueName(_ base: String) -> String {
        let names = Set(all.map(\.name))
        guard names.contains(base) else { return base }
        var n = 2
        while names.contains("\(base) \(n)") { n += 1 }
        return "\(base) \(n)"
    }

    // MARK: Persistence

    struct FilePreset: Codable, Equatable {
        var id: UUID
        var name: String
        var groups: [String]
        var relative: [String]
        /// Exact fields (absent in version-1 files).
        var absolute: [String]?
        var edit: EditSnapshot
        var autoApplyOnImport: Bool
    }

    struct File: Codable, Equatable {
        var version = 1
        var presets: [FilePreset]
    }

    static func encode(_ presets: [Preset]) throws -> Data {
        let file = File(presets: presets.map {
            FilePreset(id: $0.id, name: $0.name, groups: $0.groups.map(\.rawValue).sorted(),
                       relative: $0.relative.map(\.rawValue).sorted(),
                       absolute: $0.absoluteFields.isEmpty ? nil : $0.absoluteFields.sorted(), edit: EditCodec.encode($0.edit),
                       autoApplyOnImport: $0.autoApplyOnImport)
        })
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        return try enc.encode(file)
    }

    static func decode(_ data: Data) throws -> [Preset] {
        try JSONDecoder().decode(File.self, from: data).presets.map(preset)
    }

    private static func preset(_ f: FilePreset) -> Preset {
        Preset(id: f.id, name: f.name, groups: Set(f.groups.compactMap(AdjustGroup.init(rawValue:))),
               relative: Set(f.relative.compactMap(AdjustGroup.init(rawValue:))),
               absoluteFields: Set(f.absolute ?? []), edit: EditCodec.decode(f.edit),
               autoApplyOnImport: f.autoApplyOnImport)
    }

    /// A damaged file's readable presets and how many weren't (N8): each complete object
    /// in the "presets" array is decoded on its own, so a truncated or partly corrupt file
    /// keeps every entry that still reads (an object cut off at the end counts as unreadable).
    static func salvage(_ data: Data) -> (presets: [Preset], unreadable: Int) {
        let bytes = [UInt8](data)
        guard let key = data.range(of: Data("\"presets\"".utf8)),
              let open = bytes[key.upperBound...].firstIndex(of: UInt8(ascii: "[")) else { return ([], 0) }
        var presets: [Preset] = [], unreadable = 0
        var depth = 0, start: Int?, inString = false, escaped = false
        for i in (open + 1)..<bytes.count {
            let c = bytes[i]
            if inString {
                if escaped { escaped = false } else if c == UInt8(ascii: "\\") { escaped = true } else if c == UInt8(ascii: "\"") { inString = false }
                continue
            }
            switch c {
            case UInt8(ascii: "\""): inString = true
            case UInt8(ascii: "{"):
                if depth == 0 { start = i }
                depth += 1
            case UInt8(ascii: "}") where depth > 0:
                depth -= 1
                guard depth == 0, let s = start else { continue }
                if let f = try? JSONDecoder().decode(FilePreset.self, from: Data(bytes[s...i])) {
                    presets.append(preset(f))
                } else {
                    unreadable += 1
                }
                start = nil
            case UInt8(ascii: "]") where depth == 0: return (presets, unreadable)
            default: break
            }
        }
        return (presets, unreadable + (start == nil ? 0 : 1))
    }

    /// A problem reading the file at launch. It stays shown after later saves (N8: a
    /// save used to clear it, hiding that presets had been lost).
    @ObservationIgnored private var loadError: String?
    /// Saving is refused while a damaged file couldn't be backed up (it would be overwritten).
    @ObservationIgnored private var saveBlocked = false

    private func load() {
        guard let fileURL, FileManager.default.fileExists(atPath: fileURL.path) else { return }
        let data: Data
        do {
            data = try Data(contentsOf: fileURL)
        } catch {
            // Unreadable on disk (permissions, I/O): don't write over what we couldn't read.
            saveBlocked = true
            loadError = "Couldn’t read presets: \(error.localizedDescription)"
            lastError = loadError
            return
        }
        if let presets = try? Self.decode(data) {
            userPresets = presets
            return
        }
        // N8: the file is damaged. Keep a copy before anything can overwrite it, and keep
        // every preset that still reads.
        let (salvaged, unreadable) = Self.salvage(data)
        userPresets = salvaged
        guard let backup = Self.backUp(fileURL, data) else {
            saveBlocked = true
            loadError = "Couldn’t read presets, and couldn’t back up the file; new presets won’t be saved until presets.json is fixed or removed."
            lastError = loadError
            return
        }
        let what = unreadable > 0 ? "\(unreadable) preset\(unreadable == 1 ? "" : "s") couldn’t be read"
                                  : "Couldn’t read presets"
        loadError = "\(what); the file was kept as \(backup.lastPathComponent)."
        lastError = loadError
    }

    /// Copy a damaged file next to itself as `presets.json.bak` (or `.bad-<date>` when a
    /// different backup is already there; never overwrites one). Returns where it went.
    static func backUp(_ url: URL, _ data: Data) -> URL? {
        let fm = FileManager.default
        var bak = url.appendingPathExtension("bak")
        if fm.fileExists(atPath: bak.path) {
            if (try? Data(contentsOf: bak)) == data { return bak } // this same file, backed up at an earlier launch
            let f = ISO8601DateFormatter()
            f.formatOptions = [.withFullDate, .withTime]
            bak = url.deletingLastPathComponent().appendingPathComponent("\(url.lastPathComponent).bad-\(f.string(from: Date()))")
        }
        do {
            try data.write(to: bak, options: .atomic)
            return bak
        } catch {
            return nil
        }
    }

    /// Atomic write (temp file + rename), per the "never lose a file" rule.
    private func persist() {
        guard let fileURL else { return }
        guard !saveBlocked else {
            lastError = loadError
            return
        }
        do {
            try FileManager.default.createDirectory(at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Self.encode(userPresets).write(to: fileURL, options: .atomic)
            lastError = loadError
        } catch {
            lastError = "Couldn’t save presets: \(error.localizedDescription)"
        }
    }
}
