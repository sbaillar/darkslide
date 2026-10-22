import CoreGraphics
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Plug-ins: the bundled looks and styles reproduce the old built-in code exactly, the
/// store's decisions reach the host, and recipe post-process steps.
@Suite("Plug-ins", .serialized)
@MainActor
struct PluginTests {
    static let repo = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
        .deletingLastPathComponent().deletingLastPathComponent()
    static let filmBorder = repo.appendingPathComponent("plugins/film-border.dsplugin")

    func tempDir() throws -> URL {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("plugins-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    /// A store over a fresh Plugins folder with only the bundled plug-ins; restores the
    /// app's content afterwards.
    func withStore(trash: @escaping @Sendable (URL) throws -> Void = { _ in },
                   _ body: (PluginStore, URL) async throws -> Void) async throws {
        let saved = PluginContent.current
        defer { PluginContent.current = saved }
        // `<support>/Plugins`: the support folder is protected from file grants.
        let user = try tempDir().appendingPathComponent("support/Plugins", isDirectory: true)
        let store = PluginStore(builtinDir: AppPaths.builtInPlugins, userDir: user, trash: trash)
        try await body(store, user)
    }

    // MARK: The migrated looks and styles

    /// The photo the golden results were recorded on (with the old built-in tables).
    static func base() -> EditState {
        var e = EditState()
        e.exposure.ev = 0.7
        e.exposure.contrast = 5
        e.exposure.saturation = 4
        e.hdr.shadows = 10
        e.hdr.blacks = -3
        e.presence.vibrance = 2
        e.detail.clarity = 12
        e.colorBalance.shadows = WheelValue(hue: 120, saturation: 0.05, lightness: 0)
        return e
    }

    struct Golden: Decodable {
        var values: [String: Double]
        var curves: [String: [[Double]]]
        var from: [String: Double]?
        var to: [String: Double]?
    }

    func golden() throws -> [String: Golden] {
        let url = try #require(Bundle.module.url(forResource: "Fixtures/looks-golden", withExtension: "json"))
        var raw = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any] ?? [:]
        raw.removeValue(forKey: "_order")
        let data = try JSONSerialization.data(withJSONObject: raw)
        return try JSONDecoder().decode([String: Golden].self, from: data)
    }

    func order() throws -> [String: [String]] {
        let url = try #require(Bundle.module.url(forResource: "Fixtures/looks-golden", withExtension: "json"))
        let raw = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any]
        return raw?["_order"] as? [String: [String]] ?? [:]
    }

    @Test func bundledLooksAndStylesApplyExactlyAsTheOldBuiltInsDid() throws {
        let gold = try golden()
        let order = try order()
        #expect(LookPack.all.map(\.id) == order["looks"])
        #expect(Preset.builtIns.map(\.id.uuidString) == order["styles"])
        #expect(Preset.builtInCategories == order["sections"])
        for look in LookPack.all {
            let out = look.apply(to: Self.base())
            let g = try #require(gold[look.id], "golden for \(look.id)")
            #expect(EditCodec.encode(out).values == g.values, "\(look.id)")
            #expect(EditCodec.encode(out).curves == g.curves, "\(look.id)")
            #expect(out.look?.from == g.from && out.look?.to == g.to, "\(look.id)")
        }
        for preset in Preset.builtIns {
            let out = preset.apply(to: Self.base())
            let g = try #require(gold[preset.id.uuidString], "golden for \(preset.name)")
            #expect(EditCodec.encode(out).values == g.values, "\(preset.name)")
            #expect(EditCodec.encode(out).curves == g.curves, "\(preset.name)")
        }
    }

    /// H7: every look and style from the pack is the same `Look`/`Preset`, field for
    /// field (id, name, summary, groups, relative, absolute fields, edit, category, order),
    /// as the Swift tables they replaced.
    @Test func bundledPresetsEqualTheOldTablesFieldForField() {
        #expect(LookPack.all.count == 18 && LegacyBuiltIns.looks.count == 18)
        #expect(Preset.builtIns.count == 20 && LegacyBuiltIns.styles.count == 20)
        for (new, old) in zip(LookPack.all, LegacyBuiltIns.looks) {
            #expect(new.id == old.id && new.name == old.name && new.summary == old.summary, "\(old.id)")
            #expect(new.preset.id == old.preset.id && new.preset.groups == old.preset.groups, "\(old.id)")
            #expect(new.preset.relative == old.preset.relative, "\(old.id)")
            #expect(new.preset.absoluteFields == old.preset.absoluteFields, "\(old.id)")
            #expect(new.preset.edit == old.preset.edit && new.preset.category == old.preset.category, "\(old.id)")
            #expect(new == old, "\(old.id)")
        }
        for (new, old) in zip(Preset.builtIns, LegacyBuiltIns.styles) {
            #expect(new == old, "\(old.name)")
        }
        #expect(Preset.builtInCategories == LegacyBuiltIns.categories)
    }

    /// Intensity 0 restores the photo exactly, for every bundled look.
    @Test func everyLookAtIntensityZeroIsThePhotoAgain() {
        let before = Self.base()
        for look in LookPack.all {
            let back = LookIntensity.set(0, on: look.apply(to: before))
            var expected = before
            expected.look = back.look
            #expect(back == expected, "\(look.id)")
        }
    }

    /// H8: the edit vocabulary the plug-in validator checks (ds-core) is the app's own.
    @Test func theRustKeyTableMatchesEditCodec() {
        let rust = editKeys()
        let swiftKeys = Set(EditCodec.scalars.map(\.0) + EditCodec.flags.map(\.0) + ["crop.aspect"])
        #expect(Set(rust.map(\.key)) == swiftKeys)
        for k in rust where k.key != "crop.aspect" {
            #expect(EditCodec.group(of: k.key)?.rawValue == k.group, "\(k.key)")
        }
        for k in rust where EditCodec.isDiscrete(k.key) { #expect(k.kind == "choice", "\(k.key)") }
        #expect(rust.filter { $0.kind == "flag" }.map(\.key).sorted() == EditCodec.flags.map(\.0).sorted())
    }

    @Test func disablingTheLooksPluginRemovesItsLooks() async throws {
        try await withStore { store, _ in
            let looks = try #require(store.plugins.first { $0.id == "darkslide.looks" })
            #expect(looks.builtIn && looks.state == .active)
            #expect(looks.contributionSummary == "1 look pack · 1 style pack")
            #expect(looks.trustLabel == "Built-in" && looks.stateLabel == "On")
            var changed = 0
            store.onContentChanged = { changed += 1 }
            await store.setEnabled("darkslide.looks", false)
            #expect(changed == 1)
            #expect(LookPack.all.isEmpty && Preset.builtIns.isEmpty)
            #expect(store.plugins.first { $0.id == "darkslide.looks" }?.stateLabel == "Off")
            await store.setEnabled("darkslide.looks", true)
            #expect(LookPack.all.count == 18)
            await store.remove("darkslide.looks")
            #expect(store.lastError?.contains("disabled, not removed") == true)
        }
    }

    /// Rapid toggles run concurrently off the main actor: their snapshots are applied in
    /// the order the host ran them, so the pane ends on the host's real state.
    @Test func rapidTogglesEndOnTheHostsState() async throws {
        try await withStore { store, _ in
            await withTaskGroup(of: Void.self) { group in
                for i in 0..<40 {
                    group.addTask { await store.setEnabled("darkslide.looks", i % 2 == 1) }
                }
            }
            let shown = store.plugins.first { $0.id == "darkslide.looks" }?.stateLabel
            let host = PluginSnapshot(store.host).plugins.first { $0.id == "darkslide.looks" }?.stateLabel
            #expect(shown != nil && shown == host)
            #expect(LookPack.all.isEmpty == (shown == "Off"))
        }
    }

    // MARK: Installing a code plug-in

    @Test func installingFilmBorderWaitsForApprovalsThenOffersItsStep() async throws {
        let trashed = Trashed()
        try await withStore(trash: { url in trashed.urls.append(url); try FileManager.default.removeItem(at: url) }) { store, user in
            let preview = try #require(await store.preview(Self.filmBorder))
            #expect(preview.name == "Film Border" && preview.hasCode && !preview.downgrade)
            #expect(PluginInstallText.signer(preview).contains("runs code"))
            #expect(PluginInstallText.adds(preview) == "Adds: Film Border")
            #expect(PluginInstallText.replaces(preview) == nil)
            let result = await store.install(preview)
            #expect(result == .installed(id: "com.example.film-border", version: "1.1.0"))
            #expect(FileManager.default.fileExists(atPath: Self.filmBorder.appendingPathComponent("border.wat").path),
                    "the source is never touched")
            let rec = try #require(store.plugins.first { $0.id == "com.example.film-border" })
            #expect(rec.state == .needsApproval && rec.approvals == [.allowUnsignedCode])
            #expect(rec.trustLabel == "Unsigned" && rec.stateLabel == "Waiting for you")
            #expect(store.postProcessors.isEmpty)
            await store.approve(rec, .allowUnsignedCode)
            #expect(!store.allowUnsignedCode, "allowing this plug-in isn't the global switch")
            let info = try #require(store.postProcessors.first)
            #expect(info.name == "Film Border" && info.params.map(\.key) == ["width", "color", "outside"])
            #expect(info.pluginVersion == "1.1.0")
            #expect(info.stepID == "com.example.film-border/border")
            // A value an update no longer allows is clamped, and the editor says so.
            var step = RecipePostProcess(info)
            step.values["width"] = .number(5)
            #expect(store.notes(for: step).first?.hasPrefix("Width:") == true)
            #expect(store.notes(for: RecipePostProcess(info)).isEmpty)
            let again = try #require(await store.preview(Self.filmBorder))
            #expect(await store.install(again) == .alreadyInstalled(id: "com.example.film-border", version: "1.1.0"))
            let cancelled = try #require(await store.preview(Self.filmBorder))
            await store.cancel(cancelled)
            await store.setAllowUnsignedCode(false)
            #expect(store.postProcessors.count == 1, "it was allowed on its own")
            await store.remove("com.example.film-border")
            #expect(trashed.urls.count == 1 && trashed.urls[0].path.hasPrefix(user.path))
            #expect(!store.plugins.contains { $0.id == "com.example.film-border" })
        }
    }

    @Test func developmentFoldersAndFileGrantsGoThroughTheStore() async throws {
        try await withStore { store, user in
            // A development copy of the Film Border with a file permission.
            let dev = try tempDir().appendingPathComponent("Frame.dsplugin")
            try FileManager.default.createDirectory(at: dev, withIntermediateDirectories: true)
            try FileManager.default.copyItem(at: Self.filmBorder.appendingPathComponent("border.wat"),
                                             to: dev.appendingPathComponent("border.wat"))
            let toml = try String(contentsOf: Self.filmBorder.appendingPathComponent("plugin.toml"), encoding: .utf8)
                .replacingOccurrences(of: "com.example.film-border", with: "com.example.frame")
                .replacingOccurrences(of: "api = 1\n", with: "api = 1\npermissions = [\"files.write:out\"]\n")
            try toml.write(to: dev.appendingPathComponent("plugin.toml"), atomically: true, encoding: .utf8)
            await store.addDevFolder(dev)
            #expect(store.lastError?.contains("Allow unsigned code") == true)
            await store.setAllowUnsignedCode(true)
            await store.addDevFolder(dev)
            let rec = try #require(store.plugins.first { $0.id == "com.example.frame" })
            #expect(rec.dev && rec.trustLabel.hasPrefix("DEV"))
            let grant = PluginApproval.grant(permission: "files.write:out")
            #expect(grant.needsFolder && !PluginApproval.allowUnsignedCode.needsFolder)
            // Never a folder holding Darkslide's own files or a session.
            let session = try tempDir()
            store.protectedRoots = { [session] }
            await store.approve(rec, grant, folder: user)
            #expect(store.lastError?.contains("Darkslide's own files") == true)
            await store.grant(rec.id, "files.write:out", folder: session)
            #expect(store.lastError != nil)
            await store.approve(rec, grant, folder: try tempDir())
            #expect(store.lastError == nil)
            #expect(store.plugins.first { $0.id == "com.example.frame" }?.state == .active)
            await store.removeDevFolder(try #require(store.devFolders.first))
            #expect(store.devFolders.isEmpty)
        }
    }

    @Test func badFoldersAndRefusedGrantsSayWhy() async throws {
        try await withStore { store, _ in
            let empty = try tempDir()
            #expect(await store.preview(empty) == nil)
            #expect(store.lastError?.contains("plugin.toml") == true)
            await store.grant("com.example.none", "clock")
            #expect(store.lastError != nil)
            await store.revoke("com.example.none", "clock")
            await store.trustSigner(key: "zz", name: "x")
            #expect(store.lastError?.contains("Ed25519") == true)
            await store.untrustSigner(key: "zz")
            await store.approveFiles("com.example.none")
            #expect(store.lastError != nil)
            await store.reload()
            #expect(PluginStore.message(DsError.Cancelled) == "Cancelled.")
            #expect(PluginStore.message(DsError.Io(message: "disk")) == "disk")
            #expect(PluginStore.message(CocoaError(.fileNoSuchFile)).isEmpty == false)
        }
    }

    @Test func approvalsReadAsSentences() {
        let all: [PluginApproval] = [.changed, .notInstalled, .trustSigner(signer: "A", key: "k", fingerprint: "AAAA-BBBB"),
                                     .allowUnsignedCode, .grant(permission: "clock")]
        #expect(all.map(\.action) == ["Approve", "Approve", "Trust Signer", "Allow", "Grant"])
        #expect(all.allSatisfy { !$0.title.isEmpty })
        #expect(all[2].title.contains("AAAA-BBBB") && all[2].title.contains("claims to be “A”"))
        let preview = PluginInstallPreview(token: "t", id: "a.b", name: "N", version: "1.0.0", summary: "",
                                           trust: .signed(signer: "S", key: "k", fingerprint: "F"),
                                           permissions: [], contributions: [], hasCode: false, replaces: "2.0.0",
                                           downgrade: true)
        #expect(PluginInstallText.signer(preview).contains("key F, which you trust"))
        #expect(PluginInstallText.adds(preview).contains("nothing"))
        #expect(PluginInstallText.replaces(preview)?.contains("older") == true)
        var up = preview
        up.downgrade = false
        up.trust = .untrusted(signer: "U", key: "k", fingerprint: "F")
        #expect(PluginInstallText.replaces(up) == "Replaces version 2.0.0.")
        #expect(PluginInstallText.signer(up).contains("trust that key"))
        up.trust = .builtIn
        #expect(PluginInstallText.signer(up) == "Part of Darkslide.")
        up.trust = .unsigned
        #expect(PluginInstallText.signer(up).contains("only data"))
    }

    @Test func recordLabelsCoverEveryState() {
        var r = PluginRecord(id: "a.b", name: "N", version: nil, summary: "", authors: [], path: "/x", builtIn: false,
                             dev: false, trust: .untrusted(signer: "U", key: "k", fingerprint: "F"), state: .shadowed,
                             message: nil, approvals: [], permissions: [], contributions: [
                                PluginContributionInfo(kind: "export_post_process", id: "a", name: "A"),
                                PluginContributionInfo(kind: "export_post_process", id: "b", name: "B"),
                                PluginContributionInfo(kind: "voice_phrases", id: "c", name: "C"),
                             ], warnings: [], hasCode: true)
        #expect(r.contributionSummary == "1 phrase pack · 2 export steps")
        #expect(r.trustLabel.contains("not trusted") && r.stateLabel.contains("Replaced"))
        r.state = .unsupported
        #expect(r.stateLabel == "Not for this Mac")
        r.state = .error
        #expect(r.stateLabel == "Didn't load")
        r.trust = .signed(signer: "S", key: "k", fingerprint: "F")
        #expect(r.trustLabel == "Key F (“S”)")
    }

    // MARK: Copy as Look JSON

    @Test func copyAsLookJSONMakesAValidLookItem() throws {
        var e = EditState()
        e.exposure.contrast = 12
        e.colorBalance.shadows = WheelValue(hue: 200, saturation: 0.1, lightness: 0)
        e.curve.points[.luma] = [CurvePoint(0.2, 0.3)]
        e.crop.angle = 3
        let json = LookJSON.item(from: e, name: "Warm Fade!")
        let obj = try #require(try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
        #expect(obj["id"] as? String == "warm-fade" && obj["name"] as? String == "Warm Fade!")
        let values = try #require(obj["values"] as? [String: Double])
        #expect(values == ["exposure.contrast": 12, "cb.shadows.hue": 200, "cb.shadows.saturation": 0.1])
        #expect(LookJSON.leftOut(e) == ["curves", "the crop"])
        #expect(LookJSON.leftOut(EditState()).isEmpty)
        #expect(LookJSON.id(for: "!!!") == "look")
    }

    // MARK: Voice phrases from plug-ins

    @Test func pluginPhrasesYieldToTheUsersOwn() {
        let plugin = ["keeper": "key-pick", "bin it": "key-reject", "photo finish": "key-pick"]
        let book = VoicePhraseBook.effective(user: ["Keeper": "key-reject"], plugin: plugin)
        #expect(book.phrases["Keeper"] == "key-reject", "the user's phrase wins")
        #expect(book.phrases["bin it"] == "key-reject")
        #expect(book.phrases["keeper"] == nil && book.phrases["photo finish"] == nil, "clashing and unheard phrases are dropped")
    }

    @Test func phrasePacksThatClashOrAreNotEnglishAreLeftOut() {
        var c = PluginContent()
        func pack(_ name: String, _ json: String) -> PluginDataPack {
            PluginDataPack(pluginId: "p.\(name)", pluginName: name, contributionId: "v", json: json)
        }
        c.add(phrases: [
            pack("A", #"{"language":"en","phrases":{"keeper":"key-pick","bin it":"key-reject"}}"#),
            pack("B", #"{"language":"en-GB","phrases":{"Keeper":"key-reject"}}"#),
            pack("C", #"{"language":"de","phrases":{"behalten":"key-pick"}}"#),
            pack("D", "not json"),
        ])
        #expect(c.phrases == ["bin it": "key-reject"])
        #expect(c.errors.contains { $0.contains("“keeper” is claimed by “A” and “B”") })
        #expect(c.errors.contains { $0.contains("voice commands are English only") })
        #expect(c.errors.contains { $0.hasPrefix("“D”") })
    }

    // MARK: Recipe steps

    func info(_ params: [PluginParamInfo]) -> PostProcessorInfo {
        PostProcessorInfo(pluginId: "p.q", pluginName: "P", pluginVersion: "1.0.0", contributionId: "c", name: "Step",
                          params: params)
    }

    func param(_ key: String, _ kind: String, min: Double? = nil, max: Double? = nil, _ def: String) -> PluginParamInfo {
        PluginParamInfo(key: key, label: key.capitalized, kind: kind, min: min, max: max, choices: [], maxLen: nil,
                        defaultJson: def)
    }

    @Test func recipeStepsToggleAndCarryTheirValues() throws {
        let width = param("width", "float", min: 0, max: 0.2, "0.03")
        let count = param("count", "int", min: 1, max: 5, "2")
        let on = param("on", "bool", "true")
        let label = param("label", "text", "\"hi\"")
        let step = info([width, count, on, label])
        var r = ExportRecipe(name: "Web")
        #expect(!r.runs(step))
        r.setPostProcess(step, on: true)
        r.setPostProcess(step, on: true)
        #expect(r.postProcess.count == 1 && r.runs(step))
        var s = r.postProcess[0]
        #expect(s.id == "p.q/c" && s.number(width) == 0.03 && s.number(count) == 2 && s.bool(on) && s.text(label) == "hi")
        s.setNumber(9, for: width)
        s.setNumber(2.6, for: count)
        s.set(.bool(false), for: on)
        #expect(s.number(width) == 0.2 && s.number(count) == 3 && !s.bool(on))
        let spec = s.spec
        let decoded = try JSONDecoder().decode([String: PluginParamValue].self, from: Data(spec.paramsJson.utf8))
        #expect(decoded["width"] == .number(0.2) && decoded["on"] == .bool(false) && spec.pluginId == "p.q")
        #expect(r.missingPostProcess(available: []).count == 1)
        #expect(r.versionNote(r.postProcess[0], available: [step]) == nil)
        var newer = step
        newer.pluginVersion = "2.0.0"
        #expect(r.versionNote(r.postProcess[0], available: [newer])?.contains("Set up with P 1.0.0; 2.0.0") == true)
        #expect(r.missingPostProcess(available: [step]).isEmpty)
        r.setPostProcess(step, on: false)
        #expect(r.postProcess.isEmpty)
    }

    @Test func colourParamsRoundTripThroughHex() {
        let color = param("color", "color", "\"#102030\"")
        var s = RecipePostProcess(info([color]))
        #expect(s.color(color).components?.map { ($0 * 255).rounded() } == [16, 32, 48, 255])
        s.setColor(CGColor(srgbRed: 1, green: 0, blue: 0, alpha: 0.5), for: color)
        #expect(s.text(color) == "#FF000080")
        s.setColor(CGColor(srgbRed: 0, green: 0, blue: 1, alpha: 1), for: color)
        #expect(s.text(color) == "#0000FF")
        #expect(RecipePostProcess.rgba("#zz0000") == nil && RecipePostProcess.rgba("red") == nil)
        s.set(.text("nope"), for: color)
        #expect(s.color(color).alpha == 1)
    }

    @Test func recipesKeepTheirStepsThroughCodingAndTheSpec() throws {
        var r = ExportRecipe(name: "Web")
        r.setPostProcess(info([]), on: true)
        let back = try JSONDecoder().decode(ExportRecipe.self, from: JSONEncoder().encode(r))
        #expect(back.postProcess == r.postProcess)
        #expect(r.summary.contains("Step"))
        let spec = RustExportService.spec(r, defaultFolder: URL(fileURLWithPath: "/tmp"))
        #expect(spec.postProcess.map(\.contributionId) == ["c"])
        // A recipe saved before plug-ins decodes with none.
        var json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(ExportRecipe(name: "Old"))) as? [String: Any]
        json?.removeValue(forKey: "postProcess")
        let old = try JSONDecoder().decode(ExportRecipe.self, from: JSONSerialization.data(withJSONObject: json ?? [:]))
        #expect(old.postProcess.isEmpty)
    }

    @Test func paramValuesCodeAsPlainJSON() throws {
        let values: [PluginParamValue] = [.number(1.5), .bool(true), .text("x")]
        let data = try JSONEncoder().encode(values)
        #expect(String(data: data, encoding: .utf8) == "[1.5,true,\"x\"]")
        #expect(try JSONDecoder().decode([PluginParamValue].self, from: data) == values)
        #expect(PluginParamValue(json: "{") == nil)
    }

    @Test func builtInPluginsComeFromTheSourceTreeOutsideTheApp() throws {
        let dir = try #require(AppPaths.builtInPlugins)
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("darkslide-looks.dsplugin/plugin.toml").path))
        #expect(AppPaths.plugins.path.hasPrefix(AppPaths.support.path))
    }
}

/// What a test's trash closure saw.
private final class Trashed: @unchecked Sendable {
    var urls: [URL] = []
}
