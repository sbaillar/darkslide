import DarkslideFFI
import Foundation
import Synchronization

/// What the active plug-ins add to the app: looks, styles and voice phrases, decoded
/// from their packs (ds-plugin normalizes and validates them). Read from any thread;
/// `PluginStore` replaces it whenever plug-ins change. Until then it holds what the
/// app's one host (`PluginHosts.shared`) had at first use.
public struct PluginContent: Sendable {
    public var looks: [Look] = []
    /// Styles in pack order; `styleSections` in order of first appearance.
    public var styles: [Preset] = []
    public var styleSections: [String] = []
    /// Phrase → voice command id (the user's own phrases win over these).
    public var phrases: [String: String] = [:]
    /// Packs that didn't load, and items dropped for clashing with earlier ones.
    public var errors: [String] = []

    public init() {}

    private static let storage = Mutex<PluginContent?>(nil)

    /// The current content (loading the plug-ins on first use).
    public static var current: PluginContent {
        get {
            if let c = storage.withLock({ $0 }) { return c }
            let loaded = load(from: PluginHosts.shared)
            return storage.withLock { stored in
                if stored == nil { stored = loaded }
                return stored!
            }
        }
        set { storage.withLock { $0 = newValue } }
    }

    /// Decode every active pack of `host`.
    public static func load(from host: PluginHost) -> PluginContent {
        var c = PluginContent()
        let looks = host.dataPacks(kind: .looks)
        let styles = host.dataPacks(kind: .styles)
        let phrases = host.dataPacks(kind: .voicePhrases)
        c.errors = looks.errors + styles.errors + phrases.errors
        for pack in looks.packs { c.add(looks: pack) }
        for pack in styles.packs { c.add(styles: pack) }
        c.add(phrases: phrases.packs)
        return c
    }

    mutating func add(looks pack: PluginDataPack) {
        guard let items = decode([PackLook].self, pack) else { return }
        for item in items {
            guard let uuid = UUID(uuidString: item.uuid) else { continue }
            if looks.contains(where: { $0.id == item.id || $0.preset.id == uuid }) {
                errors.append("“\(pack.pluginName)”: the look \(item.name) is already provided by another plug-in")
                continue
            }
            looks.append(item.look(uuid: uuid))
        }
    }

    mutating func add(styles pack: PluginDataPack) {
        guard let items = decode([PackStyle].self, pack) else { return }
        for item in items {
            guard let uuid = UUID(uuidString: item.uuid) else { continue }
            if styles.contains(where: { $0.id == uuid }) {
                errors.append("“\(pack.pluginName)”: the style \(item.name) is already provided by another plug-in")
                continue
            }
            styles.append(item.preset(uuid: uuid))
            if !styleSections.contains(item.section) { styleSections.append(item.section) }
        }
    }

    /// Phrases from every pack: one claimed by two packs is left out (both lose it), and
    /// packs not in English (voice commands' language) are listed, not used.
    mutating func add(phrases packs: [PluginDataPack]) {
        var claims: [String: [(pack: String, command: String)]] = [:]
        for pack in packs {
            guard let p = decode(PackPhrases.self, pack) else { continue }
            guard p.language.lowercased().hasPrefix("en") else {
                errors.append("“\(pack.pluginName)”: its phrases are in \(p.language); voice commands are English only")
                continue
            }
            for (phrase, command) in p.phrases {
                claims[VoiceCommandParser.normalize(phrase), default: []].append((pack.pluginName, command))
                if claims[VoiceCommandParser.normalize(phrase)]!.count == 1 { phrases[phrase] = command }
            }
        }
        for (key, owners) in claims where owners.count > 1 {
            phrases = phrases.filter { VoiceCommandParser.normalize($0.key) != key }
            errors.append("“\(key)” is claimed by \(owners.map { "“\($0.pack)”" }.joined(separator: " and ")); neither is used")
        }
    }

    private mutating func decode<T: Decodable>(_ type: T.Type, _ pack: PluginDataPack) -> T? {
        do {
            return try JSONDecoder().decode(type, from: Data(pack.json.utf8))
        } catch {
            errors.append("“\(pack.pluginName)”: \(error.localizedDescription)")
            return nil
        }
    }
}

/// A look (ds-plugin `look@1`): amounts only, so Intensity scales it exactly.
private struct PackLook: Decodable {
    var id: String
    var uuid: String
    var name: String
    var summary: String?
    var values: [String: Double]?
    var exact: [String]?

    /// As the built-in looks were made: the groups it changes, tone groups relative,
    /// its `exact` keys and every setting of each effect it uses set exactly.
    func look(uuid: UUID) -> Look {
        let e = EditCodec.decode(EditSnapshot(values: values ?? [:]))
        let preset = Preset(id: uuid, name: name, groups: AdjustGroup.changed(in: e),
                            relative: [.exposure, .presence, .hdr],
                            absoluteFields: Set(exact ?? []).union(LookPack.effectFields(e)), edit: e,
                            isBuiltIn: true, category: LookPack.category)
        return Look(id: id, name: name, summary: summary ?? "", preset: preset)
    }
}

/// A style (ds-plugin `style@1`): a preset with its groups, relative groups and exact
/// fields explicit.
private struct PackStyle: Decodable {
    var uuid: String
    var name: String
    var section: String
    var groups: [String]
    var relative: [String]?
    var absolute: [String]?
    var values: [String: Double]?
    var curves: [String: [[Double]]]?
    var ranges: [ColorRangeEdit]?
    var skin: SkinToneEdit?

    func preset(uuid: UUID) -> Preset {
        let e = EditCodec.decode(EditSnapshot(values: values ?? [:], curves: curves ?? [:], ranges: ranges, skin: skin))
        return Preset(id: uuid, name: name, groups: Set(groups.compactMap(AdjustGroup.init(rawValue:))),
                      relative: Set((relative ?? []).compactMap(AdjustGroup.init(rawValue:))),
                      absoluteFields: Set(absolute ?? []), edit: e, isBuiltIn: true, category: section)
    }
}

private struct PackPhrases: Decodable {
    var language: String
    var phrases: [String: String]
}
