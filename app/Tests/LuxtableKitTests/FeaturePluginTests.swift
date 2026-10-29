import LuxtableFFI
import Foundation
import Testing
@testable import LuxtableCore

// The app's optional integrations as bundled plug-ins (docs/PLUGINS.md, "Native
// features"): off by default, the one-time migration, each feature's gate, switching
// at runtime with work in flight, and settings surviving off and on.

// MARK: - Doubles

/// A Keychain item kept in memory.
private final class FPKey: @unchecked Sendable {
    private let lock = NSLock()
    private var value: String?
    init(_ v: String? = nil) { value = v }
    var item: KeychainItem {
        KeychainItem(read: { self.lock.withLock { self.value } },
                     write: { v in self.lock.withLock { self.value = v; return true } })
    }
}

private final class FPCount: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func add() { lock.withLock { n += 1 } }
    var value: Int { lock.withLock { n } }
}

/// Records what the Immich model asks of its service.
private final class FPImmich: ImmichService, @unchecked Sendable {
    private let lock = NSLock()
    private var _publish: [ImmichPublishConfig?] = []
    private var _syncs = 0
    var pending = 0
    var states: [Int64: ImmichCopyState] = [:]
    var publish: [ImmichPublishConfig?] { lock.withLock { _publish } }
    var syncs: Int { lock.withLock { _syncs } }

    func testConnection(server: ImmichServer) async throws -> ImmichConnection {
        ImmichConnection(userName: "Kai", userEmail: "", isAdmin: false, libraries: [], librariesNote: nil)
    }
    func detectMapping(server: ImmichServer, archiveRoot: String, knownFile: String?) async throws -> ImmichDetectedMapping? {
        nil
    }
    func syncAlbums(server: ImmichServer, mapping: ImmichPathMapping, rules: [ImmichRule], every: Bool) async throws
        -> ImmichSyncReport {
        lock.withLock { _syncs += 1; pending = 0 }
        return ImmichSyncReport(finishedMs: 0, sessions: 1, albums: [], missing: 0, scannedLibraries: 0, scanNote: nil,
                                failures: [], error: nil, offline: false, keyRefused: false)
    }
    func setPublish(_ config: ImmichPublishConfig?) async { lock.withLock { _publish.append(config) } }
    func pendingAlbumSyncs() async -> Int { lock.withLock { pending } }
    func mark(_ ids: [PhotoID], developed: Bool, in session: SessionInfo) async throws -> ImmichMarkReport {
        ImmichMarkReport(changed: UInt32(ids.count), keptFiles: [])
    }
    func developed(in session: SessionInfo, options: ImmichDevelopOptions) async throws -> [ImmichDeveloped] {
        lock.withLock { states.map { ImmichDeveloped(variantId: $0.key, state: $0.value, path: nil) } }
    }
    func plan(in session: SessionInfo, options: ImmichDevelopOptions) async throws -> ImmichDevelopPlan {
        ImmichDevelopPlan(stagingDir: "/tmp", jobs: [])
    }
    func rendered(_ job: ImmichDevelopJob, staged: URL, in session: SessionInfo) async throws -> URL? { nil }
}

/// Google Photos that uploads whatever it is given.
private final class FPGoogle: GooglePhotosService, @unchecked Sendable {
    private let lock = NSLock()
    private var _uploads = 0
    var uploads: Int { lock.withLock { _uploads } }
    var isSignedIn: Bool { true }
    func albums() async throws -> [GooglePhotosAlbum] { [GooglePhotosAlbum(id: "a", title: "A")] }
    func upload(_ files: [GooglePhotosFile], to target: GooglePhotosTarget,
                progress: @escaping @Sendable (GooglePhotosProgress) -> Void) async throws -> GooglePhotosUpload {
        lock.withLock { _uploads += 1 }
        return GooglePhotosUpload(uploaded: files.map(\.url))
    }
    func authorizationURL(redirectURI: String, challenge: String, state: String) throws -> URL { URL(string: "https://x.example")! }
    func code(fromRedirect target: String, state: String) throws -> String { "" }
    func exchange(code: String, verifier: String, redirectURI: String) async throws -> String { "" }
    func revoke(refreshToken: String) async {}
}

// MARK: - Helpers

private func pluginsDir() throws -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("fp-\(UUID().uuidString)/support/Plugins",
                                                                          isDirectory: true)
    try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

@MainActor
private func store(_ user: URL) -> PluginStore {
    PluginStore(builtinDir: AppPaths.builtInPlugins, userDir: user, trash: { _ in })
}

private let allOff: Set<NativeFeature> = []

// MARK: - Tests

extension PluginStoreSuites {
    @Suite("Feature plug-ins", .serialized)
    @MainActor
    struct FeaturePluginTests {
        @Test func featuresKnowTheirPlugins() {
            #expect(NativeFeature.allCases.map(\.pluginID)
                == ["luxtable.archive", "luxtable.amazon-photos", "luxtable.immich", "luxtable.google-photos"])
            for f in NativeFeature.allCases {
                #expect(NativeFeature(pluginID: f.pluginID) == f && !f.title.isEmpty && f.id == f.rawValue)
            }
            #expect(NativeFeature(pluginID: "luxtable.looks") == nil && NativeFeature(pluginID: "com.example.immich") == nil)
        }

        @Test func gatesReportEachChangeAndHidePanels() async {
            let gates = FeatureGates()
            #expect(NativeFeature.allCases.allSatisfy(gates.isOn), "unbound: every feature (the demo, tests)")
            var heard: [String] = []
            gates.onChange = { f, on in heard.append("\(f.rawValue)=\(on)") }
            gates.update([.archive, .immich])
            #expect(heard == ["amazon-photos=false", "google-photos=false"])
            gates.update([.archive, .immich])
            #expect(heard.count == 2, "no change, no call")
            #expect(gates.shows(.archive) && !gates.shows(.amazonPhotos) && gates.shows(.exposure))
            await gates.set(.amazonPhotos, true)
            await gates.set(.archive, false)
            #expect(gates.enabled == [.amazonPhotos, .immich])
            #expect(!gates.shows(.archive) && gates.shows(.amazonPhotos))
            #expect(gates.store == nil)
        }

        @Test func settingsPluginsListsTheIntegrationsOffForANewUser() throws {
            let s = store(try pluginsDir())
            let integrations = s.plugins.filter(\.isIntegration)
            #expect(Set(integrations.map(\.id)) == Set(NativeFeature.allCases.map(\.pluginID)))
            for r in integrations {
                #expect(r.state == .disabled && r.builtIn && !r.summary.isEmpty && !r.uses.isEmpty, "\(r.id)")
                #expect(r.contributionSummary == "1 integration")
            }
            #expect(s.plugins.contains { $0.id == "luxtable.looks" && !$0.isIntegration && $0.state == .active })
            #expect(s.features.isEmpty)
        }

        @Test func theFirstLaunchTurnsOnWhatAnExistingUserHadSetUp() async throws {
            let user = try pluginsDir()
            let s = store(user)
            let settings = makeSettings()
            settings.archiveTarget = "/Volumes/NAS/Photos"
            settings.amazonPhotosFolder = URL(fileURLWithPath: "/tmp/Amazon Photos")
            ImmichSettings(defaults: settings.userDefaults).serverURL = "https://immich.example.org"
            // Google: only the client built into the app; never signed in.
            let google = GooglePhotosSettings(defaults: settings.userDefaults, clientSecret: FPKey().item,
                                              refreshToken: FPKey().item, builtIn: GooglePhotosClient(id: "built-in"))
            let decided = AppModel.migrateFeatures(s, settings: settings, google: google)
            #expect(decided.count == 4)
            #expect(s.features == [.archive, .amazonPhotos, .immich])
            // Once: a later launch asks nothing and changes nothing (the user's own choice is kept).
            await s.setEnabled(NativeFeature.archive.pluginID, false)
            settings.archiveTarget = "/Volumes/Other"
            #expect(AppModel.migrateFeatures(s, settings: settings, google: google).isEmpty)
            #expect(s.features == [.amazonPhotos, .immich])
            #expect(store(user).features == [.amazonPhotos, .immich], "remembered in state.json")
        }

        @Test func aNewUserStartsWithEveryIntegrationOff() throws {
            let s = store(try pluginsDir())
            let settings = makeSettings()
            let google = GooglePhotosSettings(defaults: settings.userDefaults, clientSecret: FPKey().item,
                                              refreshToken: FPKey().item, builtIn: nil)
            #expect(AppModel.migrateFeatures(s, settings: settings, google: google).count == 4)
            #expect(s.features.isEmpty)
        }

        @Test func wasSetUpReadsEachFeaturesOwnSettings() {
            let settings = makeSettings()
            let immich = ImmichSettings(defaults: settings.userDefaults, apiKey: FPKey().item)
            let token = FPKey()
            let google = GooglePhotosSettings(defaults: settings.userDefaults, clientSecret: FPKey().item,
                                              refreshToken: token.item, builtIn: GooglePhotosClient(id: "built-in"))
            func setUp(_ f: NativeFeature) -> Bool { f.wasSetUp(settings: settings, immich: immich, google: google) }
            #expect(NativeFeature.allCases.allSatisfy { !setUp($0) })
            settings.archiveTarget = "   "
            #expect(!setUp(.archive), "blank isn't a target")
            settings.archiveTarget = "/Volumes/NAS/Photos"
            #expect(setUp(.archive))
            settings.chooseAmazonFolder(URL(fileURLWithPath: "/tmp/A"))
            #expect(setUp(.amazonPhotos))
            immich.serverURL = "https://immich.example.org"
            #expect(setUp(.immich))
            _ = token.item.write("refresh")
            #expect(setUp(.googlePhotos), "signed in")
            _ = token.item.write(nil)
            google.clientIDOverride = "mine.apps.googleusercontent.com"
            #expect(setUp(.googlePhotos), "their own client")
        }

        @Test func gatesFollowSettingsPluginsAtOnce() async throws {
            let s = store(try pluginsDir())
            let model = makeModel()
            var heard: [NativeFeature] = []
            model.features.onChange = { f, _ in heard.append(f) }
            model.followPlugins(s)
            #expect(model.features.enabled.isEmpty && model.features.store === s)
            await model.features.set(.immich, true) // through the registry, remembered
            #expect(s.features == [.immich] && model.features.enabled == [.immich])
            await s.setEnabled(NativeFeature.googlePhotos.pluginID, true) // Settings ▸ Plugins
            #expect(model.features.isOn(.googlePhotos))
            await s.setEnabled(NativeFeature.googlePhotos.pluginID, false)
            #expect(!model.features.isOn(.googlePhotos))
            #expect(heard.contains(.googlePhotos) && heard.contains(.immich))
        }

        @Test func archiveOffHidesItsEntryPointsAndKeepsItsSettings() async {
            let model = makeModel()
            model.settings.archiveTarget = "/Volumes/NAS/Photos"
            model.settings.archiveIntervalMinutes = 30
            model.archivePopoverShown = true
            await model.features.set(.archive, false)
            #expect(!model.archivePopoverShown, "its popover closes")
            #expect(!model.features.shows(.archive))
            #expect(model.cull.deleteArchiveNote == CullFinisher.archiveOffNote, "never claims the archive has a copy")
            await model.features.set(.archive, true)
            #expect(model.settings.archiveTarget == "/Volumes/NAS/Photos" && model.settings.archiveIntervalMinutes == 30)
            #expect(model.cull.deleteArchiveNote != CullFinisher.archiveOffNote)
        }

        /// A Keychain read prompts again after every ad-hoc build: Immich off never reads it.
        @Test func immichOffNeverReadsTheKeychain() async {
            let model = makeModel(counts: [4])
            await model.start()
            await model.features.set(.immich, false)
            let reads = FPCount()
            let key = KeychainItem(read: { reads.add(); return "secret-key" }, write: { _ in true })
            let s = ImmichSettings(defaults: testDefaults(), apiKey: key)
            s.serverURL = "https://immich.example.org"
            s.pathOnServer = "/mnt/photos"
            s.pathOnMac = "/Volumes/photos"
            let immich = ImmichModel(settings: s)
            model.features.onChange = { f, on in if f == .immich { immich.featureChanged(on) } }
            immich.configure(service: FPImmich(), app: model, watch: true)
            immich.loadKeyState()
            immich.sessionOpened()
            await immich.refresh(render: true)
            _ = immich.isSetUp
            _ = immich.developHelp
            await immich.developSelected()
            try? await Task.sleep(for: TestTiming.bound(.milliseconds(50)))
            #expect(reads.value == 0 && !immich.hasKey, "off: the Keychain is never touched")
            await model.features.set(.immich, true)
            #expect(await waitUntil { immich.hasKey } && reads.value > 0, "on: the key is read")
            immich.stopWatching()
        }

        @Test func immichOffStopsItsWorkAndOnResumesIt() async {
            let model = makeModel(counts: [4])
            await model.start()
            _ = await waitUntil { model.photos.count == 4 }
            // Its own model over an in-memory key (never the login Keychain); the app's switch.
            let immich = ImmichModel(settings: ImmichSettings(defaults: testDefaults(), apiKey: FPKey().item))
            model.features.onChange = { f, on in if f == .immich { immich.featureChanged(on) } }
            let s = immich.settings
            s.serverURL = "https://immich.example.org"
            s.pathOnServer = "/mnt/photos"
            s.pathOnMac = "/Volumes/photos"
            _ = s.apiKeyItem.write("secret-key")
            let fake = FPImmich()
            let first = model.photos[0].id
            fake.states = [first.raw: .current]
            immich.configure(service: fake, app: model, watch: false)
            immich.pollInterval = .milliseconds(20)
            await immich.refresh(render: false)
            #expect(model.item(first)?.immichCopy == .current && immich.isSetUp)

            await model.features.set(.immich, false)
            #expect(!immich.isSetUp && !immich.canSync && !immich.canStopDeveloping)
            #expect(model.item(first)?.immichCopy == nil, "badges go")
            #expect(await waitUntil { fake.publish.last == .some(nil) }, "the archive stops writing sidecars")
            // The album watch doesn't sync while off, even with sessions waiting.
            fake.pending = 2
            immich.startWatching()
            try? await Task.sleep(for: TestTiming.bound(.milliseconds(150)))
            #expect(fake.syncs == 0)
            await immich.syncAlbums(every: true)
            #expect(fake.syncs == 0)
            // Nothing was deleted: the server, key and mapping are still there.
            #expect(s.isSetUp && s.apiKey == "secret-key")

            await model.features.set(.immich, true)
            #expect(immich.isSetUp)
            #expect(await waitUntil { fake.publish.last == .some(s.publishConfig) }, "publishing resumes")
            #expect(await waitUntil { model.item(first)?.immichCopy == .current }, "badges come back")
            #expect(await waitUntil { fake.syncs > 0 }, "the watch resumes")
            immich.stopWatching()
        }

        @Test func googlePhotosOffBlocksUploadsAndKeepsTheRecipe() async {
            let google = FPGoogle()
            var uploading = ExportRecipe(name: "Web", format: .jpeg)
            uploading.googlePhotos = GooglePhotosTarget(albumTitle: "Trip")
            let plain = ExportRecipe(name: "Plain")
            let recipes = ExportRecipeStore(defaults: testDefaults())
            recipes.save([uploading])
            var on = false
            let export = Exp6FakeExport()
            export.outcome = ExportOutcome(written: [ExportedOutput(photoID: PhotoID(1), recipeName: "Web",
                                                                    url: URL(fileURLWithPath: "/o/1.jpg"), width: 1, height: 1, bytes: 1)],
                                           failures: [], cancelled: false, jpegEncoder: "")
            let m = ExportModel(items: [ExportJobItem(photoID: PhotoID(1), url: URL(fileURLWithPath: "/s/1.ARW"), seq: 1)],
                                defaultFolder: URL(fileURLWithPath: "/tmp/out"), service: export, store: recipes,
                                googlePhotos: google, googlePhotosOn: { on })
            #expect(m.problem?.contains("turned off") == true, "never a silent skip")
            #expect(m.offersGooglePhotos(uploading), "a recipe that uploads can still turn it off")
            #expect(!m.offersGooglePhotos(plain))
            #expect(ExportModel.moreDestinationsHint.contains("Settings ▸ Plugins"))
            await m.loadGoogleAlbums()
            #expect(m.googleAlbums.isEmpty, "no request while off")
            #expect(recipes.load().first?.googlePhotos?.albumTitle == "Trip", "the recipe keeps its album")
            on = true
            #expect(m.problem == nil && m.offersGooglePhotos(plain))
            await m.loadGoogleAlbums()
            #expect(m.googleAlbums.count == 1)
            // Turned off after the export started: the files stay exported, nothing is sent.
            on = false
            await m.uploadToGooglePhotos(export.outcome, job: ExportJob(items: m.items, recipes: [uploading],
                                                                        defaultFolder: URL(fileURLWithPath: "/o")))
            #expect(google.uploads == 0)
            #expect(m.googleUploads.first?.error?.contains("turned off") == true)
            on = true
            await m.uploadToGooglePhotos(export.outcome, job: ExportJob(items: m.items, recipes: [uploading],
                                                                        defaultFolder: URL(fileURLWithPath: "/o")))
            #expect(google.uploads == 1 && m.googleUploads.first?.uploaded.count == 1)
        }

        @Test func theAppsExportSheetAsksTheGate() async {
            let model = makeModel(counts: [3])
            await model.start()
            _ = await waitUntil { model.photos.count == 3 }
            for p in model.photos { model.item(p.id)?.photo.url = URL(fileURLWithPath: "/x/\(p.id.raw).ARW") }
            model.googlePhotos = FPGoogle()
            model.selectAll()
            var r = ExportRecipe(name: "Web", format: .jpeg)
            r.googlePhotos = GooglePhotosTarget()
            let recipes = ExportRecipeStore(defaults: testDefaults())
            recipes.save([r])
            await model.features.set(.googlePhotos, false)
            model.beginExport(store: recipes)
            #expect(model.exportModel?.googlePhotosProblem?.contains("turned off") == true)
            await model.features.set(.googlePhotos, true)
            #expect(model.exportModel?.offersGooglePhotos(ExportRecipe(name: "Plain")) == true, "at once, sheet open")
            model.endExport()
        }

        @Test func onboardingOffersTheArchiveAndTurnsOnWhatWasSetUp() async {
            let settings = makeSettings()
            let gates = FeatureGates(enabled: allOff)
            let o = OnboardingModel(settings: settings, fileExists: { _ in true }, createDirectory: { _ in }, features: gates)
            #expect(o.steps == [.welcome, .familiar, .sessions, .archive], "no Amazon step while it is off")
            #expect(!o.archiveEnabled, "offered, off until the user turns it on")
            o.step = .archive
            #expect(o.isLast && o.progress == 1)
            o.back()
            #expect(o.step == .sessions)
            o.next()
            o.next()
            #expect(o.step == .archive, "no step past the last")
            #expect(o.canFinish && o.chosenFeatures.isEmpty)
            o.archiveEnabled = true
            o.archiveTarget = "/Volumes/NAS/Photos"
            await o.refreshArchiveStatus()
            #expect(o.chosenFeatures == [.archive])
            try? o.finish()
            #expect(await waitUntil { gates.isOn(.archive) })
            #expect(!gates.isOn(.amazonPhotos))

            // Amazon on (an existing user re-running Welcome): its step is back, and a folder turns nothing new on.
            let both = FeatureGates(enabled: [.amazonPhotos])
            let again = OnboardingModel(settings: makeSettings(), fileExists: { _ in true }, createDirectory: { _ in },
                                        features: both)
            #expect(again.steps.last == .amazon)
            again.amazonFolder = URL(fileURLWithPath: "/tmp/Amazon")
            #expect(again.chosenFeatures == [.amazonPhotos])
            // A share mounted: the target is suggested, never switched on.
            let suggested = OnboardingModel(settings: makeSettings(), fileExists: { $0 == "/Volumes/NAS" },
                                            createDirectory: { _ in }, isWritable: { _ in true }, features: gates)
            suggested.archiveEnabled = false
            await suggested.suggestArchiveTarget()
            #expect(suggested.archiveTarget == AppSettings.defaultArchiveTarget && !suggested.archiveEnabled)
        }

        @Test func theArchiveServiceHandsTheRegistryToRust() async throws {
            let s = store(try pluginsDir())
            let snap = ArchiveSettingsSnapshot(enabled: false, target: "", intervalMinutes: 10, amazonFolder: nil)
            let service = RustArchiveService(initial: snap, settings: { snap }, handles: { _ in nil },
                                             manifest: FileManager.default.temporaryDirectory
                                                 .appendingPathComponent("fp-\(UUID().uuidString)/m.json"),
                                             lockDir: nil, watchVolumes: false, plugins: s.host)
            #expect(service.manager != nil)
            // The manager now asks the registry; switching needs no relaunch.
            await s.setEnabled(NativeFeature.archive.pluginID, true)
            #expect(s.host.featureEnabled(feature: "archive"))
            await service.shutdown()
        }

        @Test func theSelfTestChecksTheBundledPlugins() throws {
            let good = SelfTest.bundledPlugins(AppPaths.builtInPlugins)
            #expect(good.ok, "\(good.detail)")
            #expect(good.detail.contains("luxtable.immich"))
            #expect(!SelfTest.bundledPlugins(nil).ok)
            // A bundle whose only plug-in was changed after the build: refused, integrations missing.
            let dir = try pluginsDir().deletingLastPathComponent().appendingPathComponent("Bundle", isDirectory: true)
            let copy = dir.appendingPathComponent("luxtable-immich.ltplugin", isDirectory: true)
            try FileManager.default.createDirectory(at: copy, withIntermediateDirectories: true)
            let src = try #require(AppPaths.builtInPlugins).appendingPathComponent("luxtable-immich.ltplugin/plugin.toml")
            try (String(contentsOf: src, encoding: .utf8) + "\n").write(to: copy.appendingPathComponent("plugin.toml"),
                                                                         atomically: true, encoding: .utf8)
            let bad = SelfTest.bundledPlugins(dir)
            #expect(!bad.ok && bad.detail.contains("refused") && bad.detail.contains("missing"))
        }
    }
}
