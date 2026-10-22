import CoreGraphics
import Foundation
import Testing
import DarkslideFFI
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

private let adobe = ProofProfile(name: "Adobe RGB (1998)", path: "/p/AdobeRGB1998.icc", kind: .display, model: .rgb)
private let srgb = ProofProfile(name: "sRGB IEC61966-2.1", path: "/p/srgb.icc", kind: .display, model: .rgb)

private struct Catalog: ProofProfileCatalog {
    func profiles() -> [ProofProfile] { [srgb, adobe] }
}

private struct NoTables: ProofTableBuilder {
    func table(profilePath: String, intent: ProofIntent, blackPointCompensation: Bool) throws -> ProofTable {
        ProofTable(size: 2, rgba: [Float](repeating: 0, count: 32), profileName: profilePath)
    }
}

/// The synthetic Capture One settings folder ds-c1 tests with, copied so a test can change it.
private func fixtureFolder() throws -> URL {
    let src = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
        .deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("crates/ds-c1/fixtures/settings")
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("c1settings-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let dest = dir.appendingPathComponent("Capture One")
    try FileManager.default.copyItem(at: src, to: dest)
    return dest
}

/// Capture One preferences beside `folder` (binary plist, like the real file).
private func writePrefs(near folder: URL) throws -> URL {
    let url = folder.deletingLastPathComponent().appendingPathComponent("com.captureone.captureone16.plist")
    let prefs: [String: Any] = [
        "ImportNamingFormatTokensForSession": [["name": "Image Name", "tag": 1], ["name": "Camera Counter", "tag": 11]],
        "proofProfile143": "1;Adobe RGB (1998).icm",
        "enabledRecipeNames": ["JPEG - 2048px for web use"],
        "KeyboardShortcutSet": "Default",
    ]
    try PropertyListSerialization.data(fromPropertyList: prefs, format: .binary, options: 0).write(to: url)
    return url
}

@MainActor
private struct Stores {
    let defaults = testDefaults()
    let settings: AppSettings
    let recipes: ExportRecipeStore
    let layout: ToolLayout
    let presets: PresetStore
    let ratios: CropRatioStore
    let keywords: KeywordLibrary
    let proof: SoftProofModel
    let targets: C1SettingsStores
    let journal: URL

    init() {
        settings = AppSettings(defaults: defaults)
        recipes = ExportRecipeStore(defaults: defaults)
        layout = ToolLayout(defaults: defaults)
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("c1s-\(UUID().uuidString)")
        presets = PresetStore(fileURL: dir.appendingPathComponent("presets.json"))
        ratios = CropRatioStore(defaults: defaults)
        keywords = KeywordLibrary(defaults: defaults)
        proof = SoftProofModel(store: SoftProofStore(defaults: defaults), catalog: Catalog(), builder: NoTables())
        targets = C1SettingsStores(recipes: recipes, layout: layout, presets: presets, ratios: ratios, keywords: keywords,
                                   settings: settings, softProof: proof)
        journal = dir.appendingPathComponent(C1SettingsJournal.fileName)
    }

    var importer: C1SettingsImporter { C1SettingsImporter(targets: targets, journalURL: journal) }
}

private func actions(_ plan: C1SettingsPlan, _ c: C1SettingsCategory) -> [String: C1SettingsAction] {
    Dictionary(uniqueKeysWithValues: plan.rows(in: c).map { ($0.item.title, $0.action) })
}

@MainActor
struct C1SettingsImportTests {
    @Test func dryRunImportReimportAndRevertOnTheFixtureFolder() async throws {
        let folder = try fixtureFolder()
        let prefs = try writePrefs(near: folder)
        let s = Stores()
        s.settings.renameTemplate = "IMG_{original}"
        let before = (layout: s.layout.workspace, rename: s.settings.renameTemplate)
        let scan = try await RustC1SettingsReader(catalog: Catalog()).read(folder: folder, preferences: prefs)

        // Dry run: what would happen, nothing written.
        let plan = s.importer.plan(scan)
        #expect(actions(plan, .recipes).values.allSatisfy { $0 == .create })
        #expect(plan.rows(in: .recipes).count == 3)
        #expect(actions(plan, .cropRatios) == ["Cinema": .create, "Square": .unavailable("same as Darkslide's 1:1 crop")])
        #expect(Set(actions(plan, .presets).keys) == ["Fade", "Soft Skin", "Brighter"])
        #expect(actions(plan, .keywords) == ["Travel (5 keywords)": .create])
        #expect(actions(plan, .preferences) == ["Import file names: {original}": .create,
                                                "Soft-proof profile: Adobe RGB (1998)": .create])
        #expect(plan.summary(.cropRatios) == "1 new, 1 not importable")
        #expect(s.recipes.load() == ExportRecipe.defaults && !s.importer.canRevert)
        #expect(scan.notes.contains { $0.text.contains("Camera Counter") })
        #expect(scan.skipped.contains { $0.file == "Recipes143/Broken.xml" })

        // Import every category.
        let done = try s.importer.apply(plan, categories: Set(C1SettingsCategory.allCases))
        #expect(done.changes(in: Set(C1SettingsCategory.allCases)) == 3 + 1 + 1 + 3 + 1 + 2)
        let recipes = s.recipes.load()
        let web = try #require(recipes.first { $0.name == "JPEG - 2048px for web use" })
        #expect(web.enabled && web.sizeMode == .longEdge && web.sizeValue == 2048 && web.quality == 80)
        #expect(web.subfolder == "Web" && !web.includeMetadata && web.stripGPS)
        let a3 = try #require(recipes.first { $0.name == "TIFF - A3 Print (300 dpi)" })
        #expect(!a3.enabled && a3.format == .tiff16 && a3.sizeValue == 4961)
        #expect(a3.outputProfile?.path == adobe.path && a3.destination?.path == "/tmp/Darkslide Fixture Exports")
        let psd = try #require(recipes.first { $0.name == "PSD - Logo" })
        #expect(psd.space == .displayP3 && psd.sharpen == 0, "output sharpening is never guessed")
        #expect(psd.watermark?.text == "© Darkslide" && psd.watermark?.anchor == .bottomRight)

        let saved = try #require(s.layout.saved.first)
        #expect(saved.name == "Studio (Capture One)")
        #expect(saved.workspace.tabs.map(\.id).prefix(3) == [.library, .favorites, .exposure])
        #expect(saved.workspace.panels(for: .favorites) == [.styles, .whiteBalance, .exposure, .hdr])
        #expect(saved.workspace.collapsed == [.levels])
        #expect(s.layout.workspace == before.layout, "the current layout is untouched")

        #expect(s.ratios.ratios.map(\.name) == ["Cinema"])
        let brighter = try #require(s.presets.userPresets.first { $0.name == "Brighter" })
        #expect(brighter.groups == [.exposure] && brighter.relative.isEmpty && brighter.edit.exposure.ev == 0.5)
        #expect(s.keywords.keywords == ["Places", "Coast", "Harbour", "beach", "sunset"])
        #expect(s.settings.renameTemplate == "{original}" && s.proof.settings.profile == adobe)
        #expect(s.importer.canRevert)

        // Re-import: idempotent.
        let again = s.importer.plan(scan)
        #expect(again.changes(in: Set(C1SettingsCategory.allCases)) == 0)
        #expect(again.rows.allSatisfy { $0.action == .upToDate || $0.action == .unavailable("same as Darkslide's 1:1 crop") })

        // The user changes things in Darkslide; Capture One changes a style.
        var list = s.recipes.load()
        let webIndex = try #require(list.firstIndex { $0.id == web.id })
        list[webIndex].quality = 60
        list[webIndex].enabled = false
        s.recipes.save(list)
        s.layout.deleteWorkspace(named: saved.name)
        s.keywords.remove("beach")
        let fade = folder.appendingPathComponent("Styles/Fade.costyle")
        try String(contentsOf: fade, encoding: .utf8).replacingOccurrences(of: "\"-12\"", with: "\"-30\"")
            .write(to: fade, atomically: true, encoding: .utf8)
        let changed = try await RustC1SettingsReader(catalog: Catalog()).read(folder: folder, preferences: prefs)
        let third = s.importer.plan(changed)
        #expect(actions(third, .recipes)["JPEG - 2048px for web use"] == .keptYours)
        #expect(actions(third, .workspaces)["Studio (Capture One)"] == .deletedInDarkslide)
        #expect(actions(third, .keywords)["Travel (5 keywords)"] == .keptYours)
        let fadeID = try #require(s.presets.userPresets.first { $0.name == "Fade" }?.id)
        #expect(actions(third, .presets)["Fade"] == .update(id: fadeID.uuidString))
        try s.importer.apply(third, categories: [.presets])
        let updated = try #require(s.presets.userPresets.first { $0.id == fadeID })
        #expect(updated.edit.exposure.saturation == -30)

        // Revert: what is still as imported goes; the user's change stays; preferences come back.
        s.settings.renameTemplate = "{date}_{original}"
        let r = try s.importer.revert()
        #expect(r.kept == 3, "the edited recipe, the trimmed keyword library and the changed rename template")
        #expect(r.restored == 1 && r.removed == 2 + 1 + 3)
        #expect(s.recipes.load().map(\.name) == ExportRecipe.defaults.map(\.name) + ["JPEG - 2048px for web use"])
        #expect(s.presets.userPresets.isEmpty && s.ratios.ratios.isEmpty)
        #expect(s.keywords.keywords == ["Places", "Coast", "Harbour", "sunset"])
        #expect(s.proof.settings.profile == nil && s.settings.renameTemplate == "{date}_{original}")
        #expect(before.rename == "IMG_{original}")
        #expect(!s.importer.canRevert && !FileManager.default.fileExists(atPath: s.journal.path))
        #expect(r.message.contains("6 items removed") && r.message.contains("1 preference restored")
            && r.message.contains("3 changed since and kept"))
    }

    @Test func existingItemsAreNeverOverwrittenAndUncheckedCategoriesStayOut() async throws {
        let folder = try fixtureFolder()
        let s = Stores()
        s.recipes.save([ExportRecipe(name: "jpeg - 2048PX for web use", quality: 50)])
        s.layout.storeWorkspace(ToolWorkspace.default, named: "Studio (Capture One)")
        _ = s.presets.save(name: "Fade", edit: EditState(), groups: [.exposure])
        s.keywords.add(["SUNSET"])
        s.ratios.put(CustomCropRatio(name: "cinema", width: 2, height: 1))
        let scan = try await RustC1SettingsReader(catalog: Catalog()).read(folder: folder, preferences: nil)
        let plan = s.importer.plan(scan)
        #expect(actions(plan, .recipes)["JPEG - 2048px for web use"] == .alreadyInDarkslide)
        #expect(actions(plan, .workspaces)["Studio (Capture One)"] == .alreadyInDarkslide)
        #expect(actions(plan, .presets)["Fade"] == .alreadyInDarkslide)
        #expect(actions(plan, .keywords)["Travel (5 keywords)"] == .create, "only the missing keywords are added")
        #expect(actions(plan, .cropRatios)["Cinema"] == .alreadyInDarkslide)
        #expect(plan.rows(in: .preferences).isEmpty && plan.summary(.preferences) == "none found")
        try s.importer.apply(plan, categories: [.keywords])
        #expect(s.keywords.keywords == ["SUNSET", "Places", "Coast", "Harbour", "beach"])
        #expect(s.recipes.load().map(\.quality) == [50] && s.presets.userPresets.count == 1)
    }

    @Test func journalRecordsFoldAndBadFilesReadAsEmpty() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("c1j-\(UUID().uuidString)")
        let url = dir.appendingPathComponent(C1SettingsJournal.fileName)
        #expect(C1SettingsJournal.load(url) == C1SettingsJournal())
        var j = C1SettingsJournal()
        j.record(.init(category: .preferences, key: "pref:x", id: "x", fingerprint: "1", written: Data("a".utf8),
                       before: Data("orig".utf8)))
        j.record(.init(category: .preferences, key: "pref:x", id: "x", fingerprint: "2", written: Data("b".utf8),
                       before: Data("a".utf8)))
        #expect(j.entries.count == 1 && j.entries[0].before == Data("orig".utf8) && j.entries[0].fingerprint == "2")
        try j.save(url)
        #expect(C1SettingsJournal.load(url) == j)
        try Data("{".utf8).write(to: url)
        #expect(C1SettingsJournal.load(url).entries.isEmpty)
        #expect(try Data(contentsOf: url.appendingPathExtension("bak")) == Data("{".utf8), "the damaged journal is kept aside")
        try C1SettingsJournal().save(url)
        #expect(!FileManager.default.fileExists(atPath: url.path))
        try C1SettingsJournal().save(url) // nothing to remove
    }

    @Test func labelsAndCounts() {
        #expect(C1SettingsAction.create.writes && C1SettingsAction.update(id: "x").writes && !C1SettingsAction.upToDate.writes)
        let labels = [C1SettingsAction.create, .update(id: "x"), .upToDate, .keptYours, .deletedInDarkslide,
                      .alreadyInDarkslide, .unavailable("why")].map(\.label)
        #expect(Set(labels).count == 7 && labels.last == "why")
        #expect(C1SettingsCategory.allCases.map(\.title).count == 6)
        #expect(C1SettingsCategory.keywords.count(1) == "1 keyword library" && C1SettingsCategory.presets.count(2) == "2 presets")
        #expect(C1SettingsCategory.allCases.allSatisfy { !$0.noun.0.isEmpty })
        #expect(C1SettingsRevert().message == "Capture One settings import reverted: 0 items removed")
        let item = C1SettingsItem(category: .keywords, key: "k", title: "k", fingerprint: "k", payload: .keywords(["k"]))
        #expect(C1SettingsScan(folder: URL(fileURLWithPath: "/x"), items: [item]).items(in: .keywords) == [item])
        #expect(C1Preference.renameTemplate("a").id == "renameTemplate")
    }
}

/// The sheet's model with a scripted reader.
private final class ScriptedReader: C1SettingsReading, @unchecked Sendable {
    var result: Result<C1SettingsScan, Error>
    private(set) var reads: [(URL, URL?)] = []
    init(_ result: Result<C1SettingsScan, Error>) { self.result = result }

    func read(folder: URL, preferences: URL?) async throws -> C1SettingsScan {
        reads.append((folder, preferences))
        return try result.get()
    }
}

private struct Boom: LocalizedError { var errorDescription: String? { "unreadable folder" } }

@MainActor
struct C1SettingsModelTests {
    func scan() -> C1SettingsScan {
        C1SettingsScan(folder: URL(fileURLWithPath: "/c1"), items: [
            C1SettingsItem(category: .keywords, key: "keywords:Lib.txt", title: "Lib (1 keywords)", fingerprint: "a",
                           payload: .keywords(["a"])),
            C1SettingsItem(category: .cropRatios, key: "ratio:r", title: "Wide", fingerprint: "w",
                           payload: .cropRatio(CustomCropRatio(name: "Wide", width: 3, height: 1))),
        ])
    }

    @Test func previewImportAndRevert() async {
        let s = Stores()
        let reader = ScriptedReader(.success(scan()))
        let m = C1SettingsImportModel(folder: URL(fileURLWithPath: "/c1"), preferences: URL(fileURLWithPath: "/p.plist"),
                                      reader: reader, importer: s.importer)
        #expect(m.phase == .scanning && m.preview == nil && !m.canImport && !m.canRevert)
        await m.run() // nothing to run yet
        await m.load()
        #expect(m.preview?.rows.count == 2 && m.canImport)
        m.categories = []
        #expect(!m.canImport)
        m.categories = [.keywords]
        await m.run()
        guard case .done(let plan) = m.phase else { Issue.record("not done: \(m.phase)"); return }
        #expect(m.doneLines(plan) == ["Keyword libraries: 1 new"])
        #expect(s.keywords.keywords == ["a"] && s.ratios.ratios.isEmpty && m.canRevert)
        m.revert()
        #expect(m.phase == .reverted(C1SettingsRevert(removed: 1)) && !m.canRevert && s.keywords.keywords.isEmpty)
        m.revert() // nothing left
        await m.choose(folder: URL(fileURLWithPath: "/other"))
        #expect(m.folder.path == "/other" && m.preferences == nil && reader.reads.last?.1 == nil)
        #expect(m.categories == Set(C1SettingsCategory.allCases))
    }

    @Test func failuresAreShown() async {
        let s = Stores()
        let reader = ScriptedReader(.failure(Boom()))
        let m = C1SettingsImportModel(folder: URL(fileURLWithPath: "/c1"), preferences: nil, reader: reader, importer: s.importer)
        await m.load()
        #expect(m.phase == .failed("unreadable folder"))
        reader.result = .failure(CancellationError())
        await m.load()
        #expect(m.phase == .failed("Cancelled"))
        #expect(m == m && m != C1SettingsImportModel(folder: m.folder, preferences: nil, reader: reader, importer: s.importer))
    }

    @Test func menuOpensTheSheetWithTheDefaultFolder() async {
        let m = makeModel()
        let reader = ScriptedReader(.success(scan()))
        m.c1SettingsReader = reader
        m.showCaptureOneSettingsImport()
        guard case .captureOneSettings(let sheet) = m.sheet else { Issue.record("no sheet"); return }
        #expect(sheet.folder.path.hasSuffix("Library/Application Support/Capture One"))
        #expect(m.sheet?.id == "capture-one-settings")
        _ = await waitUntil { sheet.preview != nil }
        #expect(reader.reads.count == 1)
    }
}

struct C1SettingsReaderTests {
    @Test func preferencesFileIsTheNewestVersion() throws {
        #expect(RustC1SettingsReader.preferencesFile(in: [
            "com.captureone.captureone16.plist", "com.captureone.captureone9.plist", "com.captureone.captureone.plist",
            "com.captureone.captureoneX.plist", "other.plist", "com.captureone.captureone16.plist.lockfile",
        ]) == "com.captureone.captureone16.plist")
        #expect(RustC1SettingsReader.preferencesFile(in: ["a.plist"]) == nil)
        let home = FileManager.default.temporaryDirectory.appendingPathComponent("home-\(UUID().uuidString)")
        let prefs = home.appendingPathComponent("Library/Preferences")
        try FileManager.default.createDirectory(at: prefs, withIntermediateDirectories: true)
        try Data().write(to: prefs.appendingPathComponent("com.captureone.captureone12.plist"))
        let loc = RustC1SettingsReader.defaultLocations(home: home)
        #expect(loc.folder.path == home.appendingPathComponent("Library/Application Support/Capture One").path)
        #expect(loc.preferences?.lastPathComponent == "com.captureone.captureone12.plist")
        #expect(RustC1SettingsReader.defaultLocations(home: home.appendingPathComponent("none")).preferences == nil)
    }

    @Test func profilesMatchByNameOrFile() {
        let list = [srgb, adobe]
        #expect(C1SettingsScan.profile(named: "adobe rgb (1998)", in: list) == adobe)
        #expect(C1SettingsScan.profile(named: "AdobeRGB1998", in: list) == adobe)
        #expect(C1SettingsScan.profile(named: "ProPhoto", in: list) == nil)
    }
}

@MainActor
struct C1SettingsTargetTests {
    @Test func keywordLibraryAndCropRatioStoresPersist() {
        let d = testDefaults()
        let k = KeywordLibrary(defaults: d)
        k.add(["beach", "Beach", " sunset "])
        #expect(k.keywords == ["beach", "sunset"] && KeywordLibrary(defaults: d).keywords == ["beach", "sunset"])
        #expect(k.suggestions(excluding: ["BEACH"]) == ["sunset"] && k.suggestions(excluding: [], limit: 1) == ["beach"])
        k.remove("BEACH")
        #expect(k.keywords == ["sunset"] && k.contains("Sunset"))

        let r = CropRatioStore(defaults: d)
        let wide = CustomCropRatio(name: "Wide", width: 2.39, height: 1)
        r.put(wide)
        r.put(CustomCropRatio(id: wide.id, name: "Cinema", width: 2.39, height: 1))
        #expect(CropRatioStore(defaults: d).ratios.map(\.name) == ["Cinema"] && r.ratio(wide.id)?.name == "Cinema")
        r.remove(wide.id)
        #expect(r.ratios.isEmpty)
        d.set(Data("x".utf8), forKey: CropRatioStore.key)
        #expect(CropRatioStore(defaults: d).ratios.isEmpty)
        #expect(CustomCropRatio(name: "x", width: 0, height: 1).ratio == nil)
        #expect(abs((wide.ratio ?? 0) - 2.39) < 1e-9)
    }

    @Test func storeHooks() {
        let s = Stores()
        #expect(s.layout.storeWorkspace(.default, named: " ") == nil)
        #expect(s.layout.storeWorkspace(.default, named: "A") == "A" && s.layout.storeWorkspace(.default, named: "a") == "a")
        #expect(s.layout.saved.map(\.name) == ["a"])
        let p = s.presets.add(Preset(name: "Look", groups: [.exposure], edit: EditState()))
        let p2 = s.presets.add(Preset(name: "Look", groups: [.curve], edit: EditState()))
        #expect(p2.name == "Look 2")
        s.presets.setAutoApply(p.id)
        s.presets.replace(p.id, with: Preset(name: "Look 2", groups: [.hdr], edit: EditState()))
        let replaced = s.presets.userPresets.first { $0.id == p.id }
        #expect(replaced?.name == "Look 2 2" && replaced?.groups == [.hdr] && replaced?.autoApplyOnImport == true)
        s.presets.replace(UUID(), with: p) // unknown id: nothing
        s.proof.setProfile(adobe)
        s.proof.setProfile(nil)
        #expect(s.proof.settings.profile == nil)
        // Targets: unknown ids and the no-op paths.
        #expect(s.targets.snapshot(.preferences, id: "nope") == nil)
        #expect(s.targets.snapshot(.cropRatios, id: "not-a-uuid") == nil)
        s.targets.restore(.recipes, id: "x", to: Data())
        s.targets.restore(.preferences, id: "nope", to: Data())
        s.targets.remove(.preferences, id: "renameTemplate")
        s.targets.remove(.cropRatios, id: "bad")
        s.targets.remove(.presets, id: "bad")
        let keyword = C1SettingsItem(category: .keywords, key: "k", title: "k", fingerprint: "k", payload: .keywords(["k"]))
        #expect(s.targets.before(keyword) == nil)
        #expect(s.targets.snapshot(.keywords, id: "not json") == nil)
        #expect((try? s.targets.replace("not json", with: keyword)) == #"["k"]"#, "an unreadable id starts over")
        let ws = C1SettingsItem(category: .workspaces, key: "w", title: "w", fingerprint: "w",
                                payload: .workspace(name: "Default", .default))
        #expect(throws: (any Error).self) { try s.targets.create(ws) }
        #expect(C1SettingsStores.Failure.unwritable("x").errorDescription?.contains("x") == true)
        let recipe = C1SettingsItem(category: .recipes, key: "r", title: "r", fingerprint: "r",
                                    payload: .recipe(ExportRecipe(name: "r")))
        _ = try? s.targets.replace(UUID().uuidString, with: recipe) // gone: nothing
        #expect(!s.recipes.load().contains { $0.name == "r" })
    }

    @Test func customRatioCropsTheFocusedPhotoOnce() async throws {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        _ = await waitUntil { m.viewer.frame != nil }
        let id = try #require(m.focusedID)
        let size = m.viewer.cropBox(for: m.edit(for: id)).size
        let steps = m.history.undoStack.count
        m.applyCropRatio(CustomCropRatio(name: "Pano", width: 3, height: 1))
        let box = m.viewer.cropBox(for: m.edit(for: id))
        #expect(abs((box.rect.width * size.width) / (box.rect.height * size.height) - 3) < 1e-3, "\(box.rect)")
        #expect(m.edit(for: id).crop.aspect == .free && m.history.undoStack.count == steps + 1)
        m.applyCropRatio(CustomCropRatio(name: "Bad", width: 0, height: 0))
        #expect(m.history.undoStack.count == steps + 1)
    }
}

struct C1SettingsConversionTests {
    @Test func workspacesDropEmptyTabsAndNoteUnknownTools() {
        let item = C1SettingsScan.workspace(C1WorkspaceRecord(
            file: "W", name: "W (Capture One)",
            tabs: [C1WorkspaceTabRecord(c1Id: "QuickToolTab", builtin: "favorites", title: "", panels: ["exposure", "exposure", "nope"]),
                   C1WorkspaceTabRecord(c1Id: "CaptureToolTab", builtin: nil, title: "Capture", panels: []),
                   C1WorkspaceTabRecord(c1Id: "BWToolTab", builtin: nil, title: "  ", panels: ["curve"])],
            collapsed: ["curve", "bogus"], unknownTools: ["Capture: Camera"], notes: [], fingerprint: "f"))
        guard case .workspace(let name, let w) = item.payload else { Issue.record("not a workspace"); return }
        #expect(name == "W (Capture One)" && w.tabs.count == 2 && w.panels(for: .favorites) == [.exposure])
        #expect(w.tabs[1].id.isCustom && w.tabs[1].title == "Custom" && w.collapsed == [.curve])
        #expect(item.notes.map(\.key) == ["tools", "tabs"] && item.notes[1].text.contains("Capture"))
        #expect(item.unavailable == nil)
        let empty = C1SettingsScan.workspace(C1WorkspaceRecord(file: "E", name: "E", tabs: [], collapsed: [],
                                                               unknownTools: [], notes: [], fingerprint: "e"))
        #expect(empty.unavailable != nil)
    }

    @Test func recipesStylesAndPreferences() {
        let r = C1RecipeRecord(
            file: "f", name: "Print", enabled: true, format: .jpeg, quality: 90, size: .percent(percent: 50),
            allowUpscale: false, displayP3: false, profileName: "Printer Matte", sharpen: 0, fileNameTemplate: "{name}",
            destination: nil, subfolder: "", includeMetadata: true, stripGps: false,
            watermark: C1WatermarkRecord(text: nil, logo: "/l.png", font: "", color: 0xFFFFFF, opacity: 0.5, anchor: "nowhere"),
            notes: [], fingerprint: "x")
        let item = C1SettingsScan.recipe(r, profiles: [adobe])
        guard case .recipe(let recipe) = item.payload else { Issue.record("not a recipe"); return }
        #expect(recipe.sizeMode == .percent && recipe.sizeValue == 50 && recipe.outputProfile == nil)
        #expect(recipe.watermark?.kind == .logo && recipe.watermark?.anchor == .bottomRight)
        #expect(item.notes.first?.text.contains("isn't installed") == true)
        for (size, mode) in [(C1RecipeSize.original, ExportRecipe.SizeMode.original), (.shortEdge(px: 9), .shortEdge)] {
            var rr = r
            rr.size = size
            guard case .recipe(let x) = C1SettingsScan.recipe(rr, profiles: []).payload else { continue }
            #expect(x.sizeMode == mode)
        }
        let text = C1SettingsScan.watermark(C1WatermarkRecord(text: "Hi", logo: nil, font: "Futura", color: 1, opacity: 1,
                                                               anchor: "topLeft"))
        #expect(text.kind == .text && text.text == "Hi" && text.anchor == .topLeft && text.fontFamily == "Futura")

        let style = C1SettingsScan.preset(C1StyleRecord(file: "s", name: "Grain", isPreset: true, category: nil,
                                                        adjustmentsJson: "{", groups: [], mapped: [], notes: [], fingerprint: "g"))
        #expect(style.unavailable == "couldn't be read" && style.group == "Presets", "never a preset that resets tools")
        let empty = C1SettingsScan.preset(C1StyleRecord(file: "e", name: "Grain", isPreset: false, category: nil,
                                                        adjustmentsJson: "{}", groups: [], mapped: [], notes: [], fingerprint: "e"))
        #expect(empty.unavailable == "nothing in it maps to Darkslide" && empty.group == "Styles")
        let ratio = C1SettingsScan.cropRatio(C1AspectRatioRecord(key: "k", name: "Sq", width: 1, height: 1, builtin: "1:1",
                                                                  notes: []))
        #expect(ratio.unavailable == "same as Darkslide's 1:1 crop")
        let prefs = C1SettingsScan.preferences(C1PrefsRecord(importNaming: nil, enabledRecipes: [], proofProfile: "Nope",
                                                             shortcutSet: nil, notes: []), profiles: [adobe])
        #expect(prefs.count == 1 && prefs[0].unavailable == "not installed on this Mac")
    }
}

/// Targets that write and then lose the item (its snapshot is gone): the import must
/// take it out again and stop.
@MainActor
private final class LosingTargets: C1SettingsTargets {
    var created: [String] = []
    var removed: [String] = []
    func snapshot(_ category: C1SettingsCategory, id: String) -> Data? { nil }
    func exists(_ item: C1SettingsItem) -> Bool { false }
    func create(_ item: C1SettingsItem) throws -> String {
        created.append(item.key)
        return item.key
    }
    func replace(_ id: String, with item: C1SettingsItem) throws -> String { id }
    func remove(_ category: C1SettingsCategory, id: String) { removed.append(id) }
    func restore(_ category: C1SettingsCategory, id: String, to data: Data) {}
    func before(_ item: C1SettingsItem) -> Data? { nil }
}

@MainActor
struct C1SettingsFailureTests {
    func ratioScan() -> C1SettingsScan {
        C1SettingsScan(folder: URL(fileURLWithPath: "/c1"), items: [
            C1SettingsItem(category: .cropRatios, key: "ratio:a", title: "A", fingerprint: "a",
                           payload: .cropRatio(CustomCropRatio(name: "A", width: 2, height: 1))),
            C1SettingsItem(category: .cropRatios, key: "ratio:b", title: "B", fingerprint: "b",
                           payload: .cropRatio(CustomCropRatio(name: "B", width: 3, height: 1))),
        ])
    }

    @Test func aJournalThatCantBeSavedTakesTheItemBackOut() throws {
        let s = Stores()
        let blocker = FileManager.default.temporaryDirectory.appendingPathComponent("c1-blocker-\(UUID().uuidString)")
        try Data("a file where the journal's folder should be".utf8).write(to: blocker)
        let importer = C1SettingsImporter(targets: s.targets, journalURL: blocker.appendingPathComponent("j.json"))
        let plan = importer.plan(ratioScan())
        #expect(throws: (any Error).self) { try importer.apply(plan, categories: [.cropRatios]) }
        #expect(s.ratios.ratios.isEmpty, "nothing is left that a revert couldn't reach")
        // A preference is put back rather than removed.
        s.settings.renameTemplate = "mine"
        let pref = C1SettingsScan(folder: URL(fileURLWithPath: "/c1"), items: [
            C1SettingsItem(category: .preferences, key: "pref:renameTemplate", title: "t", fingerprint: "x",
                           payload: .preference(.renameTemplate("{original}"))),
        ])
        #expect(throws: (any Error).self) { try importer.apply(importer.plan(pref), categories: [.preferences]) }
        #expect(s.settings.renameTemplate == "mine")
    }

    @Test func anItemThatCantBeReadBackStopsTheImport() throws {
        let t = LosingTargets()
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("c1-lose-\(UUID().uuidString)")
        let importer = C1SettingsImporter(targets: t, journalURL: dir.appendingPathComponent("j.json"))
        #expect(throws: C1SettingsError.notSaved("A")) {
            try importer.apply(importer.plan(ratioScan()), categories: [.cropRatios])
        }
        #expect(t.created == ["ratio:a"] && t.removed == ["ratio:a"] && !importer.canRevert)
        #expect(C1SettingsError.notSaved("A").errorDescription?.contains("A") == true)
    }

    @Test func aLargeKeywordLibraryIsOneQuickItem() throws {
        let s = Stores()
        s.keywords.add(["kw 7"])
        let words = (0..<5000).map { "kw \($0)" }
        let scan = C1SettingsScan(folder: URL(fileURLWithPath: "/c1"), items: [
            C1SettingsItem(category: .keywords, key: "keywords:Big.txt", title: "Big", fingerprint: "1", payload: .keywords(words)),
        ])
        let clock = ContinuousClock()
        let start = clock.now
        let plan = s.importer.plan(scan)
        try s.importer.apply(plan, categories: [.keywords])
        #expect(clock.now - start < .seconds(5))
        #expect(s.keywords.keywords.count == 5000 && s.keywords.keywords.first == "kw 7")
        // A grown library adds only what's new; a revert removes what the imports added.
        var bigger = scan
        bigger.items[0].payload = .keywords(words + ["kw new"])
        bigger.items[0].fingerprint = "2"
        let again = s.importer.plan(bigger)
        #expect(again.rows[0].action.writes)
        try s.importer.apply(again, categories: [.keywords])
        #expect(s.keywords.keywords.count == 5001)
        let r = try s.importer.revert()
        #expect(r.removed == 1 && s.keywords.keywords == ["kw 7"], "the keyword that was already there stays")
    }
}
