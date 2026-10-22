import Foundation
import Observation

/// A saved set of IPTC fields (e.g. creator + copyright): applied to the selection
/// from the Metadata panel, or to every new import (`applyOnImport`, at most one).
/// Applying it sets only the fields it has.
public struct MetadataPreset: Identifiable, Hashable, Sendable, Codable {
    public var id: UUID
    public var name: String
    public var fields: IPTCMetadata
    public var applyOnImport: Bool

    public init(id: UUID = UUID(), name: String, fields: IPTCMetadata, applyOnImport: Bool = false) {
        self.id = id
        self.name = name
        self.fields = fields
        self.applyOnImport = applyOnImport
    }
}

/// The user's metadata presets, in `metadata-presets.json` in the app's support
/// folder (atomic writes; a file that can't be read is backed up, never overwritten).
@MainActor @Observable
public final class MetadataPresetStore {
    public static let shared = MetadataPresetStore(fileURL: AppPaths.support.appendingPathComponent("metadata-presets.json"))

    public private(set) var presets: [MetadataPreset] = []
    /// The last load/save problem, shown in the panel.
    public private(set) var lastError: String?
    @ObservationIgnored private let fileURL: URL?
    @ObservationIgnored private var saveBlocked = false

    public init(fileURL: URL?) {
        self.fileURL = fileURL
        load()
    }

    /// The preset set on every new import, if any.
    public var importPreset: MetadataPreset? { presets.first(where: \.applyOnImport) }

    /// Save the set fields of `fields` as a new preset (nil: no name, or nothing set).
    @discardableResult
    public func save(name: String, fields: IPTCMetadata) -> MetadataPreset? {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !fields.isEmpty else {
            lastError = trimmed.isEmpty ? "Give the preset a name." : "Fill in at least one field first."
            return nil
        }
        var name = trimmed
        var n = 2
        while presets.contains(where: { $0.name == name }) {
            name = "\(trimmed) \(n)"
            n += 1
        }
        let p = MetadataPreset(name: name, fields: fields)
        presets.append(p)
        lastError = nil
        persist()
        return p
    }

    public func delete(_ id: UUID) {
        presets.removeAll { $0.id == id }
        lastError = nil
        persist()
    }

    /// Make `id` the import preset (`nil`: none). At most one preset has it.
    public func setImportPreset(_ id: UUID?) {
        for i in presets.indices { presets[i].applyOnImport = presets[i].id == id }
        lastError = nil
        persist()
    }

    private func load() {
        guard let fileURL, FileManager.default.fileExists(atPath: fileURL.path) else { return }
        do {
            let data = try Data(contentsOf: fileURL)
            do {
                presets = try JSONDecoder().decode([MetadataPreset].self, from: data)
            } catch {
                saveBlocked = PresetStore.backUp(fileURL, data) == nil
                lastError = saveBlocked
                    ? "Couldn’t read metadata presets, and couldn’t back up the file."
                    : "Couldn’t read metadata presets; the file was kept as a backup."
            }
        } catch {
            saveBlocked = true
            lastError = "Couldn’t read metadata presets: \(error.localizedDescription)"
        }
    }

    private func persist() {
        guard let fileURL, !saveBlocked else { return }
        do {
            try FileManager.default.createDirectory(at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            let enc = JSONEncoder()
            enc.outputFormatting = [.prettyPrinted, .sortedKeys]
            try enc.encode(presets).write(to: fileURL, options: .atomic)
        } catch {
            lastError = "Couldn’t save metadata presets: \(error.localizedDescription)"
        }
    }
}
