import Foundation

/// The app's stores as import targets: export recipes, saved workspaces, crop ratios,
/// presets, the keyword list, the import rename template and the soft-proof profile.
/// Ids are the stores' own (UUIDs, a workspace's name). A keyword library's id is the
/// JSON list of the keywords it added, so a revert takes out exactly those.
@MainActor
public final class C1SettingsStores: C1SettingsTargets {
    let recipes: ExportRecipeStore
    let layout: ToolLayout
    let presets: PresetStore
    let ratios: CropRatioStore
    let keywords: KeywordLibrary
    let settings: AppSettings
    let softProof: SoftProofModel

    public init(recipes: ExportRecipeStore, layout: ToolLayout, presets: PresetStore, ratios: CropRatioStore,
                keywords: KeywordLibrary, settings: AppSettings, softProof: SoftProofModel) {
        self.recipes = recipes
        self.layout = layout
        self.presets = presets
        self.ratios = ratios
        self.keywords = keywords
        self.settings = settings
        self.softProof = softProof
    }

    enum Failure: LocalizedError {
        case unwritable(String)
        var errorDescription: String? {
            if case .unwritable(let what) = self { return "\u{201c}\(what)\u{201d} couldn't be saved" }
            return nil
        }
    }

    static func encode<T: Encodable>(_ v: T) -> Data? {
        let enc = JSONEncoder()
        enc.outputFormatting = .sortedKeys
        return try? enc.encode(v)
    }

    /// A recipe as compared: whether it's ticked for export is the user's to change.
    static func recipeSnapshot(_ r: ExportRecipe) -> Data? {
        var r = r
        r.enabled = false
        return encode(r)
    }

    public func snapshot(_ category: C1SettingsCategory, id: String) -> Data? {
        switch category {
        case .recipes: recipes.load().first { $0.id.uuidString == id }.flatMap(Self.recipeSnapshot)
        case .workspaces: layout.saved.first { $0.name == id }.flatMap(Self.encode)
        case .cropRatios: UUID(uuidString: id).flatMap(ratios.ratio).flatMap(Self.encode)
        case .presets: presets.userPresets.first { $0.id.uuidString == id }.flatMap { try? PresetStore.encode([$0]) }
        case .keywords: keywordSnapshot(id)
        case .preferences: preference(id)
        }
    }

    /// The keywords library `id` added that are still in the list (lowercased, sorted);
    /// nil when none are.
    private func keywordSnapshot(_ id: String) -> Data? {
        guard let added = Self.keywordList(id) else { return nil }
        let have = Set(keywords.keywords.map { $0.lowercased() })
        let present = Set(added.map { $0.lowercased() }).intersection(have).sorted()
        return present.isEmpty ? nil : Self.encode(present)
    }

    static func keywordList(_ id: String) -> [String]? { try? JSONDecoder().decode([String].self, from: Data(id.utf8)) }

    static func keywordID(_ list: [String]) -> String { String(decoding: encode(list) ?? Data("[]".utf8), as: UTF8.self) }

    /// `list`'s keywords not in the keyword list yet, added; returns them.
    private func addMissing(_ list: [String]) -> [String] {
        let have = Set(keywords.keywords.map { $0.lowercased() })
        let added = Keywords.merge([], adding: list).filter { !have.contains($0.lowercased()) }
        keywords.add(added)
        return added
    }

    private func preference(_ id: String) -> Data? {
        switch id {
        case "renameTemplate": Data(settings.renameTemplate.utf8)
        case "proofProfile": Self.encode(softProof.settings.profile)
        default: nil
        }
    }

    public func exists(_ item: C1SettingsItem) -> Bool {
        func same(_ a: String) -> Bool { a.caseInsensitiveCompare(item.name) == .orderedSame }
        switch item.payload {
        case .recipe: return recipes.load().contains { same($0.name) }
        case .workspace(let name, _): return layout.savedIndex(named: name) != nil
        case .cropRatio: return ratios.ratios.contains { same($0.name) }
        case .preset: return presets.all.contains { same($0.name) }
        case .keywords(let list):
            let have = Set(keywords.keywords.map { $0.lowercased() })
            return list.allSatisfy { have.contains($0.lowercased()) }
        case .preference(.renameTemplate(let t)): return settings.renameTemplate == t
        case .preference(.proofProfile(let p)): return softProof.settings.profile?.path == p.path
        }
    }

    public func create(_ item: C1SettingsItem) throws -> String {
        switch item.payload {
        case .recipe(var r):
            r.id = UUID()
            recipes.save(recipes.load() + [r])
            return r.id.uuidString
        case .workspace(let name, let w):
            guard let n = layout.storeWorkspace(w, named: name) else { throw Failure.unwritable(name) }
            return n
        case .cropRatio(let r):
            ratios.put(r)
            return r.id.uuidString
        case .preset(let p):
            return presets.add(p).id.uuidString
        case .keywords(let list):
            return Self.keywordID(addMissing(list))
        case .preference(let p):
            set(p)
            return p.id
        }
    }

    public func replace(_ id: String, with item: C1SettingsItem) throws -> String {
        switch item.payload {
        case .recipe(var r):
            var list = recipes.load()
            guard let i = list.firstIndex(where: { $0.id.uuidString == id }) else { return id }
            r.id = list[i].id
            r.enabled = list[i].enabled
            list[i] = r
            recipes.save(list)
        case .workspace(_, let w):
            layout.storeWorkspace(w, named: id)
        case .cropRatio(var r):
            r.id = UUID(uuidString: id) ?? r.id
            ratios.put(r)
        case .preset(let p):
            if let uuid = UUID(uuidString: id) { presets.replace(uuid, with: p) }
        case .keywords(let list):
            return Self.keywordID((Self.keywordList(id) ?? []) + addMissing(list))
        case .preference(let p):
            set(p)
        }
        return id
    }

    private func set(_ p: C1Preference) {
        switch p {
        case .renameTemplate(let t): settings.renameTemplate = t
        case .proofProfile(let profile): softProof.setProfile(profile)
        }
    }

    public func remove(_ category: C1SettingsCategory, id: String) {
        switch category {
        case .recipes: recipes.save(recipes.load().filter { $0.id.uuidString != id })
        case .workspaces: layout.deleteWorkspace(named: id)
        case .cropRatios: if let uuid = UUID(uuidString: id) { ratios.remove(uuid) }
        case .presets: if let uuid = UUID(uuidString: id) { presets.delete(uuid) }
        case .keywords: keywords.remove(Self.keywordList(id) ?? [])
        case .preferences: break // preferences are restored, never removed
        }
    }

    public func restore(_ category: C1SettingsCategory, id: String, to data: Data) {
        guard category == .preferences else { return }
        switch id {
        case "renameTemplate": settings.renameTemplate = String(decoding: data, as: UTF8.self)
        case "proofProfile": softProof.setProfile(try? JSONDecoder().decode(ProofProfile?.self, from: data))
        default: break
        }
    }

    public func before(_ item: C1SettingsItem) -> Data? {
        guard case .preference(let p) = item.payload else { return nil }
        return preference(p.id)
    }
}
