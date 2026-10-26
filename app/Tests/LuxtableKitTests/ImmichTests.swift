import LuxtableFFI
import Foundation
import Testing
@testable import LuxtableCore

// MARK: - Doubles

/// A Keychain item kept in memory.
private final class MemoryKey: @unchecked Sendable {
    private let lock = NSLock()
    private var value: String?
    var refuse = false
    init(_ v: String? = nil) { value = v }
    var item: KeychainItem {
        KeychainItem(read: { self.lock.withLock { self.value } },
                     write: { v in self.lock.withLock { if self.refuse { return false }; self.value = v; return true } })
    }
    var current: String? { lock.withLock { value } }
}

/// A scripted `ImmichService`: copies are "rendered" when the model hands them back.
private final class FakeImmich: ImmichService, @unchecked Sendable {
    private let lock = NSLock()
    var connection = ImmichConnection(userName: "Kai", userEmail: "kai@example.org", isAdmin: true, libraries: [],
                                      librariesNote: nil)
    var connectionError: Error?
    var detected: ImmichDetectedMapping?
    var report = ImmichSyncReport(finishedMs: 0, sessions: 1, albums: [], missing: 0, scannedLibraries: 1, scanNote: nil,
                                  failures: [], error: nil, offline: false, keyRefused: false)
    var syncError: Error?
    var pending = 0
    var markError: Error?
    let staging: URL
    /// Copy states by variant id.
    var states: [Int64: ImmichCopyState] = [:]
    private(set) var publish: [ImmichPublishConfig?] = []
    private(set) var syncs: [(rules: [ImmichRule], every: Bool)] = []
    private(set) var renderedIDs: [Int64] = []
    private(set) var plans = 0

    init() {
        staging = FileManager.default.temporaryDirectory.appendingPathComponent("immich-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
    }

    func testConnection(server: ImmichServer) async throws -> ImmichConnection {
        if let connectionError { throw connectionError }
        return connection
    }
    func detectMapping(server: ImmichServer, archiveRoot: String, knownFile: String?) async throws -> ImmichDetectedMapping? {
        detected
    }
    func syncAlbums(server: ImmichServer, mapping: ImmichPathMapping, rules: [ImmichRule], every: Bool) async throws
        -> ImmichSyncReport {
        lock.withLock { syncs.append((rules, every)); pending = 0 }
        if let syncError { throw syncError }
        return report
    }
    func setPublish(_ config: ImmichPublishConfig?) async { lock.withLock { publish.append(config) } }
    func pendingAlbumSyncs() async -> Int { lock.withLock { pending } }
    func mark(_ ids: [PhotoID], developed: Bool, in session: SessionInfo) async throws -> ImmichMarkReport {
        if let markError { throw markError }
        return lock.withLock {
            var kept: [String] = []
            for id in ids {
                if developed {
                    if states[id.raw] == nil { states[id.raw] = .pending }
                } else if let s = states.removeValue(forKey: id.raw), s != .pending {
                    kept.append("/archive/Developed/\(id.raw).jpg")
                }
            }
            return ImmichMarkReport(changed: UInt32(ids.count), keptFiles: kept)
        }
    }
    func developed(in session: SessionInfo, options: ImmichDevelopOptions) async throws -> [ImmichDeveloped] {
        lock.withLock { states.map { ImmichDeveloped(variantId: $0.key, state: $0.value, path: nil) } }
    }
    func plan(in session: SessionInfo, options: ImmichDevelopOptions) async throws -> ImmichDevelopPlan {
        lock.withLock {
            plans += 1
            return ImmichDevelopPlan(stagingDir: staging.path, jobs: states.filter { $0.value != .current }.map {
                ImmichDevelopJob(variantId: $0.key, originalPath: "/x/\($0.key).ARW", rel: "Developed/\($0.key).jpg",
                                 fingerprint: "f")
            })
        }
    }
    func rendered(_ job: ImmichDevelopJob, staged: URL, in session: SessionInfo) async throws -> URL? {
        lock.withLock {
            renderedIDs.append(job.variantId)
            states[job.variantId] = .current
        }
        return URL(fileURLWithPath: "/archive/\(job.rel)")
    }
    func makeOutOfDate(_ id: Int64) { lock.withLock { states[id] = .outOfDate } }
}

/// The export writes one file per item.
private final class WritingExport: ExportService, @unchecked Sendable {
    private(set) var jobs: [ExportJob] = []
    var fail: Error?
    func validate(_ recipes: [ExportRecipe], defaultFolder: URL) -> String? { nil }
    func previewFileName(template: String, sample: ExportFileNameSample, seq: Int, ext: String) -> String? { nil }
    func nameSample(for url: URL) async -> ExportFileNameSample? { nil }
    func export(_ job: ExportJob, progress: @escaping @Sendable (ExportProgressUpdate) -> Void) async throws -> ExportOutcome {
        jobs.append(job)
        if let fail { throw fail }
        return ExportOutcome(written: job.items.map {
            ExportedOutput(photoID: $0.photoID, recipeName: "Immich", url: job.defaultFolder.appendingPathComponent("\($0.seq).jpg"),
                           width: 4, height: 2, bytes: 4)
        }, failures: [], cancelled: false, jpegEncoder: "mozjpeg")
    }
}

@MainActor
private func immichSettings(key: String? = "secret-key") -> (ImmichSettings, MemoryKey) {
    let k = MemoryKey(key)
    let s = ImmichSettings(defaults: testDefaults(), apiKey: k.item)
    return (s, k)
}

@MainActor
private func setUp(_ s: ImmichSettings) {
    s.serverURL = "https://immich.example.org"
    s.pathOnServer = "/mnt/photos"
    s.pathOnMac = "/Volumes/photos"
}

/// A model with a session open, photos with files, and Immich set up on a fake.
@MainActor
private func immichModel() async -> (AppModel, FakeImmich, WritingExport) {
    let m = makeModel(counts: [6])
    await m.start()
    _ = await waitUntil { m.photos.count == 6 }
    for p in m.photos { m.item(p.id)?.photo.url = URL(fileURLWithPath: "/x/\(p.id.raw).ARW") }
    setUp(m.immich.settings)
    _ = m.immich.settings.apiKeyItem.write("secret-key")
    let fake = FakeImmich()
    let export = WritingExport()
    m.exporter = export
    m.immich.renderDelay = .milliseconds(10)
    m.immich.configure(service: fake, app: m, watch: false)
    return (m, fake, export)
}

// MARK: - Settings

@MainActor
@Suite struct ImmichSettingsTests {
    @Test func defaultsAndSetup() {
        let (s, _) = immichSettings(key: nil)
        #expect(!s.hasServer && !s.isSetUp && s.publishConfig == nil)
        #expect(s.xmpSidecars && s.labelTags && s.quality == 92 && s.longEdge == 0)
        #expect(s.rules == [.fiveStars])
        s.serverURL = "  https://immich.example.org  "
        #expect(s.serverURL == "https://immich.example.org")
        #expect(!s.hasServer, "no key yet")
        _ = s.apiKeyItem.write("k")
        #expect(s.hasServer && !s.isSetUp)
        s.pathOnServer = "/mnt/photos"
        s.pathOnMac = "/Volumes/photos"
        #expect(s.isSetUp)
        #expect(s.mapping == ImmichPathMapping(immichRoot: "/mnt/photos", localRoot: "/Volumes/photos"))
        s.xmpSidecars = false
        s.labelTags = false
        #expect(s.publishConfig == ImmichPublishConfig(xmpSidecars: false, labelTags: false))
        s.quality = 300
        s.longEdge = 2560
        #expect(s.quality == 100)
        #expect(s.developOptions == ImmichDevelopOptions(quality: 100, longEdge: 2560, labelTags: false))
        s.longEdge = -5
        #expect(s.developOptions.longEdge == nil)
        s.pathOnMac = ""
        #expect(!s.isSetUp && s.publishConfig == nil)
        let rules = [ImmichAlbumRule(album: "Picks", pickedOnly: true), ImmichAlbumRule(album: "Red", label: .red)]
        s.rules = rules
        #expect(s.rules == rules)
        s.lastSync = "Immich: albums up to date"
        #expect(s.lastSync == "Immich: albums up to date")
    }

    @Test func rulesSummaryAndFFI() {
        #expect(ImmichAlbumRule.fiveStars.summary == "★★★★★")
        #expect(ImmichAlbumRule.fiveStars.hasCriterion)
        var r = ImmichAlbumRule(album: " Coast ", minRating: 3, pickedOnly: true, label: .green, keyword: " beach ")
        #expect(r.summary == "★★★ or more · Picked · Green · “beach”")
        let f = r.ffi
        #expect(f.album == "Coast" && f.minRating == 3 && f.pick == .picked)
        #expect(f.labels == [.green] && f.keywords == ["beach"])
        r = ImmichAlbumRule(album: "All", label: ColorTag.none, keyword: "  ")
        #expect(!r.hasCriterion && r.summary == "No criterion (skipped)")
        #expect(r.ffi.labels.isEmpty && r.ffi.keywords.isEmpty && r.ffi.pick == nil)
    }

    @Test func badgesInWords() {
        #expect(ImmichCopyBadge(.current) == .current && ImmichCopyBadge.current.symbol == "photo.badge.checkmark")
        #expect(ImmichCopyBadge(.outOfDate).help.contains("out of date"))
        #expect(ImmichCopyBadge(.pending).help.contains("not made yet"))
        #expect(ImmichError.notSetUp.errorDescription?.contains("Settings ▸ Immich") == true)
        #expect(ImmichError.notArchivable("why").errorDescription == "why")
        #expect(ImmichError.failed("x").errorDescription == "x")
    }
}

// MARK: - Settings ▸ Immich

@MainActor
@Suite struct ImmichSettingsModelTests {
    @Test func saveKeepsTheKeyInTheKeychain() async {
        let (s, key) = immichSettings(key: nil)
        let model = ImmichModel(settings: s)
        let fake = FakeImmich()
        model.configure(service: fake, app: makeModel(counts: [1]), watch: false)
        #expect(!model.hasKey && !model.isSetUp && !model.canSave)
        model.serverEntry = "https://immich.example.org"
        model.keyEntry = " my-key "
        model.pathOnServerEntry = "/mnt/photos"
        model.pathOnMacEntry = "/Volumes/photos"
        #expect(model.canSave)
        await model.save()
        #expect(key.current == "my-key" && model.keyEntry.isEmpty && model.hasKey && model.message == "Saved.")
        #expect(s.defaults.string(forKey: "immichServerURL") == "https://immich.example.org")
        #expect(!s.defaults.dictionaryRepresentation().values.contains { "\($0)".contains("my-key") }, "never in preferences")
        #expect(model.isSetUp && fake.publish.last == ImmichPublishConfig(xmpSidecars: true, labelTags: true))
        key.refuse = true
        model.keyEntry = "other"
        await model.save()
        #expect(model.message == "The Keychain refused the API key.")
        // A Keychain that won't delete the key: it stays, and the model says so.
        let published = fake.publish.count
        await model.removeKey()
        #expect(key.current == "my-key" && model.hasKey)
        #expect(model.message == "The API key couldn't be removed from your Keychain.")
        #expect(fake.publish.count == published)
        key.refuse = false
        await model.removeKey()
        #expect(key.current == nil && !model.hasKey && model.message == "API key removed.")
        #expect(fake.publish.last == .some(nil), "the archive stops publishing")
    }

    @Test func connectionTestAndDetection() async {
        let (s, _) = immichSettings()
        let app = makeModel(counts: [1])
        let model = ImmichModel(settings: s)
        await model.testConnection()
        #expect(model.message?.contains("server address") == true, "no service yet")
        let fake = FakeImmich()
        model.configure(service: fake, app: app, watch: false)
        await model.testConnection()
        #expect(model.message?.contains("server address") == true, "no server yet")
        s.serverURL = "https://immich.example.org"
        fake.connection.librariesNote = "Only an admin's API key can see the libraries."
        fake.connection.libraries = [ImmichLibrary(id: "l", name: "NAS", importPaths: ["/mnt/photos"], exclusionPatterns: [])]
        await model.testConnection()
        #expect(model.message == "Connected as Kai (admin). Only an admin's API key can see the libraries.")
        #expect(model.libraries.count == 1)
        fake.connection.userName = ""
        fake.connection.isAdmin = false
        await model.testConnection()
        #expect(model.message?.hasPrefix("Connected as kai@example.org.") == true)
        fake.connectionError = ImmichError.failed("Immich refused the API key: Invalid API key")
        await model.testConnection()
        #expect(model.message == "Immich refused the API key: Invalid API key")
        // Detection needs the archive folder.
        app.settings.archiveTarget = ""
        await model.detectMapping()
        #expect(model.message?.contains("archive folder") == true)
        let archive = FileManager.default.temporaryDirectory.appendingPathComponent("immich-archive-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: archive.appendingPathComponent("Shoot/Capture"),
                                                 withIntermediateDirectories: true)
        FileManager.default.createFile(atPath: archive.appendingPathComponent("Shoot/Capture/a.ARW").path, contents: Data())
        defer { try? FileManager.default.removeItem(at: archive) }
        #expect(ImmichModel.archivedFile(under: archive)?.lastPathComponent == "a.ARW")
        #expect(ImmichModel.archivedFile(under: archive.appendingPathComponent("none")) == nil)
        app.settings.archiveTarget = archive.path
        await model.detectMapping()
        #expect(model.message?.contains("enter both folders") == true)
        fake.detected = ImmichDetectedMapping(mapping: ImmichPathMapping(immichRoot: "/mnt/photos", localRoot: "/Volumes/photos"),
                                              how: "asset")
        await model.detectMapping()
        #expect(model.pathOnServerEntry == "/mnt/photos" && model.pathOnMacEntry == "/Volumes/photos")
        #expect(model.message?.contains("archived photo") == true)
        fake.detected?.how = "library"
        await model.detectMapping()
        #expect(model.message?.contains("import path") == true)
    }
}

// MARK: - Developed copies

@MainActor
@Suite(.serialized) struct ImmichDevelopTests {
    @Test func developingSelectedPhotosRendersAndBadgesThem() async throws {
        let (m, fake, export) = await immichModel()
        let ids = Array(m.photos.prefix(2).map(\.id))
        m.selection = Set(ids)
        #expect(CommandCatalog.isEnabled(.developForImmich, model: m))
        #expect(!CommandCatalog.isEnabled(.stopDevelopingForImmich, model: m))
        #expect(CommandCatalog.title(.developForImmich, model: m) == "Keep Developed Copies for Immich (2 Photos)")
        #expect(CommandCatalog.help(.developForImmich, model: m)?.contains("Developed folder") == true)
        CommandCatalog.perform(.developForImmich, model: m)
        #expect(await waitUntil { fake.renderedIDs.count == 2 })
        #expect(await waitUntil { ids.allSatisfy { m.item($0)?.immichCopy == .current } })
        let job = try #require(export.jobs.first)
        #expect(job.recipes.count == 1 && job.recipes[0].format == .jpeg && job.recipes[0].quality == 92)
        #expect(job.recipes[0].sizeMode == .original && job.recipes[0].labelTags)
        #expect(job.defaultFolder.standardizedFileURL.path == fake.staging.standardizedFileURL.path)
        #expect(Set(job.items.map(\.photoID)) == Set(ids))
        // Edited or re-rated: out of date, rendered again after the quiet time.
        m.selection = [ids[0]]
        #expect(CommandCatalog.title(.developForImmich, model: m) == "Keep a Developed Copy for Immich")
        fake.makeOutOfDate(ids[0].raw)
        m.rate(ids[0], 4)
        #expect(await waitUntil { fake.renderedIDs.count == 3 })
        #expect(await waitUntil { m.item(ids[0])?.immichCopy == .current })
        #expect(export.jobs.last?.items.first?.rating == 4, "the copy carries the new rating")
        // Stop: the copy stays, and the toast says so.
        #expect(CommandCatalog.isEnabled(.stopDevelopingForImmich, model: m))
        #expect(CommandCatalog.title(.stopDevelopingForImmich, model: m) == "Stop Updating Developed Copy")
        #expect(CommandCatalog.help(.stopDevelopingForImmich, model: m)?.contains("stay") == true)
        CommandCatalog.perform(.stopDevelopingForImmich, model: m)
        #expect(await waitUntil { m.item(ids[0])?.immichCopy == nil })
        #expect(m.toast?.message.contains("developed copy stays in Developed and in the archive") == true)
        #expect(m.item(ids[1])?.immichCopy == .current)
    }

    @Test func developingNeedsSetupAndReportsFailures() async {
        let (m, fake, export) = await immichModel()
        m.selection = [m.photos[0].id]
        m.immich.settings.pathOnMac = ""
        #expect(!m.immich.canDevelop)
        #expect(m.immich.developHelp.contains("Settings ▸ Immich"))
        await m.immich.developSelected()
        #expect(m.toast?.message.contains("Settings ▸ Immich") == true && m.toast?.isError == true)
        setUp(m.immich.settings)
        fake.markError = ImmichError.notArchivable("on a card: import it to archive it")
        await m.immich.developSelected()
        #expect(m.toast?.message == "on a card: import it to archive it")
        fake.markError = nil
        export.fail = ImmichError.failed("disk full")
        await m.immich.developSelected()
        #expect(await waitUntil { m.toast?.message.contains("Developed copies not made: disk full") == true })
        #expect(m.item(m.photos[0].id)?.immichCopy == .pending)
        // Stopping before a copy was made.
        fake.markError = ImmichError.failed("busy")
        await m.immich.stopDeveloping()
        #expect(m.toast?.message == "busy")
        fake.markError = nil
        await m.immich.stopDeveloping()
        #expect(m.toast?.message == "Stopped. No developed copy had been made yet.")
        // Not set up: badges are cleared.
        m.immich.settings.serverURL = ""
        await m.immich.refresh(render: false)
        #expect(m.photos.allSatisfy { m.item($0.id)?.immichCopy == nil })
    }

    @Test func videosAndPhotosWithoutFilesAreLeftOut() async {
        let (m, _, _) = await immichModel()
        let id = m.photos[0].id
        m.item(id)?.photo.url = nil
        m.selection = [id]
        #expect(!m.immich.canDevelop && !m.immich.canStopDeveloping)
    }
}

// MARK: - Albums

@MainActor
@Suite(.serialized) struct ImmichAlbumTests {
    @Test func syncReportsInWords() async {
        let (m, fake, _) = await immichModel()
        m.immich.retryDelay = .seconds(3600)
        fake.report.albums = [ImmichAlbumResult(album: "Luxtable ★★★★★", created: true, added: 3, removed: 1, missing: 2)]
        fake.report.missing = 2
        fake.report.failures = ["x"]
        #expect(m.immich.canSync)
        await m.immich.syncAlbums(every: true)
        let s = m.immich.lastSync ?? ""
        #expect(s.hasPrefix("Immich: 3 added, 1 removed · 2 not in Immich yet · 1 problem"), "\(s)")
        #expect(!m.immich.lastSyncFailed && m.immich.settings.lastSync == s)
        #expect(fake.syncs.last?.every == true && fake.syncs.last?.rules.first?.minRating == 5)
        fake.report = ImmichSyncReport(finishedMs: 0, sessions: 1, albums: [], missing: 0, scannedLibraries: 0, scanNote: nil,
                                       failures: [], error: nil, offline: false, keyRefused: false)
        await m.immich.syncAlbums(every: false)
        #expect(m.immich.lastSync?.hasPrefix("Immich: albums up to date") == true)
        fake.report.error = "Immich server not reachable: offline"
        fake.report.offline = true
        await m.immich.syncAlbums(every: false)
        #expect(m.immich.lastSync == "Immich: server not reachable; albums sync when it is back" && m.immich.lastSyncFailed)
        fake.report.offline = false
        fake.report.keyRefused = true
        await m.immich.syncAlbums(every: false)
        #expect(m.immich.lastSync?.contains("refused the API key") == true)
        fake.report.keyRefused = false
        fake.report.error = "Immich answered 500: boom"
        await m.immich.syncAlbums(every: false)
        #expect(m.immich.lastSync == "Immich: Immich answered 500: boom")
        fake.syncError = ImmichError.failed("Archiving isn't running.")
        await m.immich.syncAlbums(every: true)
        #expect(m.immich.lastSync == "Immich: Archiving isn't running." && m.immich.lastSyncFailed)
        m.immich.stopWatching()
    }

    @Test func archivedSessionsAreSyncedAndNotIndexedOnesRetried() async {
        let (m, fake, _) = await immichModel()
        m.immich.pollInterval = .milliseconds(20)
        m.immich.retryDelay = .milliseconds(30)
        fake.report.missing = 1
        fake.pending = 1
        m.immich.startWatching()
        #expect(await waitUntil { fake.syncs.contains { !$0.every } }, "an archive run's sessions are synced")
        #expect(await waitUntil { fake.syncs.contains { $0.every } }, "photos Immich hasn't indexed are tried again")
        m.immich.stopWatching()
    }

    @Test func nothingIsSyncedUntilSetUp() async {
        let (s, _) = immichSettings()
        let model = ImmichModel(settings: s)
        let fake = FakeImmich()
        model.configure(service: fake, app: makeModel(counts: [1]), watch: false)
        #expect(!model.canSync)
        await model.syncAlbums(every: true)
        #expect(fake.syncs.isEmpty && model.lastSync == nil)
        model.settingsChanged()
        #expect(await waitUntil { fake.publish.count >= 2 })
    }
}

// MARK: - The live service over the FFI

/// Canned Immich answers, like `URLSessionImmichHTTP` would get them.
private final class CannedHTTP: ImmichHttp, @unchecked Sendable {
    func send(request: ImmichHttpRequest) throws -> ImmichHttpResponse {
        let path = URL(string: request.url)?.path ?? ""
        let key = request.headers.first { $0.name.lowercased() == "x-api-key" }?.value
        func json(_ s: String, _ status: UInt16 = 200) -> ImmichHttpResponse {
            ImmichHttpResponse(status: status, headers: [], body: Data(s.utf8))
        }
        if path.hasSuffix("/server/ping") { return json(#"{"res":"pong"}"#) }
        guard key == "good" else { return json(#"{"message":"Invalid API key","statusCode":401}"#, 401) }
        if path.hasSuffix("/users/me") { return json(#"{"id":"u","name":"Kai","email":"k@example.org","isAdmin":false}"#) }
        if path.hasSuffix("/libraries") { return json(#"{"message":"Forbidden","statusCode":403}"#, 403) }
        throw DsError.Io(message: "offline")
    }
}

@Suite struct RustImmichServiceTests {
    @Test func connectionOverTheFFI() async throws {
        let s = RustImmichService(http: CannedHTTP(), manager: nil, handles: { _ in nil },
                                  statePath: FileManager.default.temporaryDirectory.appendingPathComponent("immich.json"))
        let c = try await s.testConnection(server: ImmichServer(url: "https://immich.example.org", apiKey: "good"))
        #expect(c.userName == "Kai" && !c.isAdmin && c.libraries.isEmpty && c.librariesNote != nil)
        await #expect(throws: ImmichError.self) {
            _ = try await s.testConnection(server: ImmichServer(url: "https://immich.example.org", apiKey: "bad"))
        }
        let none = try await s.detectMapping(server: ImmichServer(url: "https://immich.example.org", apiKey: "good"),
                                             archiveRoot: "/Volumes/photos/Luxtable", knownFile: nil)
        #expect(none == nil, "a non-admin key without a known file can't tell")
        await #expect(throws: ImmichError.failed("Archiving isn't running.")) {
            _ = try await s.syncAlbums(server: ImmichServer(url: "https://immich.example.org", apiKey: "good"),
                                       mapping: ImmichPathMapping(immichRoot: "/mnt", localRoot: "/Volumes"), rules: [],
                                       every: true)
        }
        #expect(await s.pendingAlbumSyncs() == 0)
        await s.setPublish(nil)
        let session = SessionInfo(id: UUID(), name: "Shoot", url: URL(fileURLWithPath: "/tmp/Shoot"), photoCount: 0,
                                  createdAt: Date())
        await #expect(throws: ImmichError.self) { _ = try await s.mark([PhotoID(1)], developed: true, in: session) }
        await #expect(throws: ImmichError.self) {
            _ = try await s.developed(in: session, options: ImmichDevelopOptions(quality: 90, longEdge: nil, labelTags: true))
        }
    }

    @Test func urlSessionHTTPRefusesOtherSchemes() {
        let h = URLSessionImmichHTTP()
        #expect(throws: DsError.self) {
            _ = try h.send(request: ImmichHttpRequest(method: "GET", url: "ftp://immich.example.org/api", headers: [], body: Data()))
        }
        #expect(RustImmichService.error(.Cancelled) is CancellationError)
        #expect(RustImmichService.error(.Unsupported(message: "card")) as? ImmichError == .notArchivable("card"))
    }
}
