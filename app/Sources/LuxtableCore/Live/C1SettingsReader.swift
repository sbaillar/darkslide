import LuxtableFFI
import Foundation

/// lt-c1 through the FFI (`read_capture_one_settings`), plus the installed ICC
/// profiles the recipes and the proof profile are matched against.
public struct RustC1SettingsReader: C1SettingsReading {
    let catalog: any ProofProfileCatalog

    public init(catalog: any ProofProfileCatalog = ColorSyncProfileCatalog()) {
        self.catalog = catalog
    }

    public func read(folder: URL, preferences: URL?) async throws -> C1SettingsScan {
        let record = try await RustSessionStore.call { token in
            try await readCaptureOneSettings(folder: folder.path, prefs: preferences?.path, token: token)
        }
        let catalog = catalog
        let profiles = await Task.detached(priority: .userInitiated) { catalog.profiles() }.value
        return C1SettingsScan(record, folder: folder, profiles: profiles)
    }

    /// Capture One's settings folder and preferences file in `home`.
    public static func defaultLocations(home: URL = PlatformWorkspace.homeDirectory)
        -> (folder: URL, preferences: URL?) {
        let folder = home.appendingPathComponent("Library/Application Support/Capture One", isDirectory: true)
        let prefsDir = home.appendingPathComponent("Library/Preferences", isDirectory: true)
        let names = (try? FileManager.default.contentsOfDirectory(atPath: prefsDir.path)) ?? []
        return (folder, preferencesFile(in: names).map { prefsDir.appendingPathComponent($0) })
    }

    /// The newest `com.captureone.captureone<N>.plist` among `names`.
    static func preferencesFile(in names: [String]) -> String? {
        let prefix = "com.captureone.captureone"
        return names.compactMap { n -> (Int, String)? in
            guard n.hasPrefix(prefix), n.hasSuffix(".plist") else { return nil }
            let mid = n.dropFirst(prefix.count).dropLast(".plist".count)
            return (Int(mid) ?? (mid.isEmpty ? 0 : -1), n)
        }
        .filter { $0.0 >= 0 }
        .max { $0.0 < $1.0 }?.1
    }
}

extension C1Note {
    init(_ r: C1NoteRecord) { self.init(key: r.key, text: r.text, dropped: r.dropped) }
}

extension C1SettingsScan {
    /// The FFI's findings as importable items; `profiles` are the installed ICC profiles.
    init(_ r: C1AppSettingsRecord, folder: URL, profiles: [ProofProfile]) {
        self.init(folder: folder)
        let exportable = profiles.filter(\.exportable)
        items += r.recipes.map { Self.recipe($0, profiles: exportable) }
        items += r.workspaces.map(Self.workspace)
        items += r.aspectRatios.map(Self.cropRatio)
        items += r.styles.map(Self.preset)
        items += r.keywordLibraries.map {
            C1SettingsItem(category: .keywords, key: "keywords:\($0.file)", title: "\($0.name) (\($0.keywords.count) keywords)",
                           fingerprint: $0.fingerprint, payload: .keywords($0.keywords))
        }
        if let p = r.prefs {
            items += Self.preferences(p, profiles: profiles)
            notes += p.notes.map(C1Note.init)
        }
        folders = r.folders.map { Folder(name: $0.folder, files: Int($0.files)) }
        skipped = r.skipped.map { Skipped(file: $0.file, reason: $0.reason) }
    }

    /// An installed profile by name ("Adobe RGB (1998)"): its description, or its file
    /// name, ignoring case, spaces and punctuation.
    static func profile(named name: String, in profiles: [ProofProfile]) -> ProofProfile? {
        func key(_ s: String) -> String { s.lowercased().filter { $0.isLetter || $0.isNumber } }
        let k = key(name)
        return profiles.first { key($0.name) == k }
            ?? profiles.first { key(URL(fileURLWithPath: $0.path).deletingPathExtension().lastPathComponent) == k }
    }

    static func recipe(_ r: C1RecipeRecord, profiles: [ProofProfile]) -> C1SettingsItem {
        var notes = r.notes.map(C1Note.init)
        let (mode, value): (ExportRecipe.SizeMode, Double) = switch r.size {
        case .original: (.original, 0)
        case .longEdge(let px): (.longEdge, Double(px))
        case .shortEdge(let px): (.shortEdge, Double(px))
        case .percent(let p): (.percent, Double(p))
        }
        var outputProfile: RecipeOutputProfile?
        if let name = r.profileName {
            if let p = profile(named: name, in: profiles) {
                outputProfile = RecipeOutputProfile(name: p.name, path: p.path, intent: .relativeColorimetric,
                                                    blackPointCompensation: true)
            } else {
                notes.append(C1Note(key: "outputProfile",
                                    text: "ICC profile \u{201c}\(name)\u{201d} isn't installed on this Mac: exports are sRGB",
                                    dropped: true))
            }
        }
        let recipe = ExportRecipe(
            name: r.name, enabled: r.enabled, format: r.format == .tiff16 ? .tiff16 : .jpeg, quality: Int(r.quality),
            sizeMode: mode, sizeValue: value, allowUpscale: r.allowUpscale, space: r.displayP3 ? .displayP3 : .sRGB,
            sharpen: Double(r.sharpen), fileNameTemplate: r.fileNameTemplate,
            destination: r.destination.map { URL(fileURLWithPath: $0, isDirectory: true) }, subfolder: r.subfolder,
            includeMetadata: r.includeMetadata, stripGPS: r.stripGps, outputProfile: outputProfile,
            watermark: r.watermark.map(watermark))
        return C1SettingsItem(category: .recipes, key: "recipe:\(r.file)", title: r.name, fingerprint: r.fingerprint,
                              notes: notes, payload: .recipe(recipe))
    }

    static func watermark(_ w: C1WatermarkRecord) -> ExportWatermark {
        var m = ExportWatermark(kind: w.logo == nil ? .text : .logo, fontFamily: w.font, color: w.color,
                                logo: w.logo.map { URL(fileURLWithPath: $0) },
                                anchor: ExportWatermark.Anchor(rawValue: w.anchor) ?? .bottomRight,
                                opacity: Double(w.opacity))
        if let t = w.text { m.text = t }
        return m
    }

    static func workspace(_ r: C1WorkspaceRecord) -> C1SettingsItem {
        var notes = r.notes.map(C1Note.init)
        var tabs: [ToolWorkspace.Tab] = []
        var empty: [String] = []
        for t in r.tabs {
            let panels = ToolWorkspace.unique(t.panels.compactMap(ToolPanelID.init(rawValue:)))
            let builtin = t.builtin.flatMap(ToolTab.init(rawValue:))
            guard !panels.isEmpty else {
                empty.append(builtin?.title ?? t.title)
                continue
            }
            if let builtin {
                tabs.append(.init(id: builtin, panels: panels))
            } else {
                let id = ToolTab.newCustom()
                tabs.append(.init(id: id, title: ToolWorkspace.cleanTitle(t.title) ?? "Custom", symbol: id.symbol, panels: panels))
            }
        }
        if !r.unknownTools.isEmpty {
            notes.append(C1Note(key: "tools", text: "No Luxtable tool for: \(r.unknownTools.joined(separator: ", "))",
                                dropped: true))
        }
        if !empty.isEmpty {
            notes.append(C1Note(key: "tabs", text: "Tabs left out (no Luxtable tools in them): \(empty.joined(separator: ", "))",
                                dropped: true))
        }
        let w = ToolWorkspace(tabs: tabs, collapsed: Set(r.collapsed.compactMap(ToolPanelID.init(rawValue:))))
        return C1SettingsItem(category: .workspaces, key: "workspace:\(r.file)", title: r.name, fingerprint: r.fingerprint,
                              notes: notes, payload: .workspace(name: r.name, w),
                              unavailable: tabs.isEmpty ? "no Luxtable tools in it" : nil)
    }

    static func cropRatio(_ r: C1AspectRatioRecord) -> C1SettingsItem {
        let ratio = CustomCropRatio(name: r.name, width: r.width, height: r.height)
        return C1SettingsItem(category: .cropRatios, key: "ratio:\(r.key)", title: r.name,
                              fingerprint: "\(r.name)|\(r.width)|\(r.height)", notes: r.notes.map(C1Note.init),
                              payload: .cropRatio(ratio),
                              unavailable: r.builtin.map { "same as Luxtable's \($0) crop" })
    }

    static func preset(_ r: C1StyleRecord) -> C1SettingsItem {
        // An unreadable result is never a preset: it would reset the tools it names.
        let edit = try? AdjustmentsCodec.decode(r.adjustmentsJson).0
        let preset = Preset(name: r.name, groups: Set(r.groups.map(AdjustGroup.init(rust:))), edit: edit ?? EditState())
        let unavailable = edit == nil ? "couldn't be read" : preset.groups.isEmpty ? "nothing in it maps to Luxtable" : nil
        return C1SettingsItem(category: .presets, key: "style:\(r.file)", title: r.name,
                              group: r.category ?? (r.isPreset ? "Presets" : "Styles"), fingerprint: r.fingerprint,
                              notes: r.notes.map(C1Note.init), payload: .preset(preset), unavailable: unavailable)
    }

    static func preferences(_ p: C1PrefsRecord, profiles: [ProofProfile]) -> [C1SettingsItem] {
        var out: [C1SettingsItem] = []
        if let t = p.importNaming {
            out.append(C1SettingsItem(category: .preferences, key: "pref:renameTemplate", title: "Import file names: \(t)",
                                      fingerprint: t, payload: .preference(.renameTemplate(t))))
        }
        if let name = p.proofProfile {
            let found = profile(named: name, in: profiles)
            let stand = found ?? ProofProfile(name: name, path: "", kind: .display, model: .rgb)
            out.append(C1SettingsItem(category: .preferences, key: "pref:proofProfile", title: "Soft-proof profile: \(name)",
                                      fingerprint: name, payload: .preference(.proofProfile(stand)),
                                      unavailable: found == nil ? "not installed on this Mac" : nil))
        }
        return out
    }
}
