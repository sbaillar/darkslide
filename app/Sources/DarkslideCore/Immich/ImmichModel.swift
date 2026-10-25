import DarkslideFFI
import Foundation
import Observation

/// Immich: the photos the user chose get a developed JPEG in the session's
/// `Developed/` folder (rendered by the export, kept current as they are edited,
/// re-rated or re-tagged), the archive carries it to the NAS that Immich indexes,
/// and the rule albums are synced after each archive run. Settings ▸ Immich edits
/// `settings`; the browser shows `PhotoItem.immichCopy`.
@MainActor @Observable
public final class ImmichModel {
    @ObservationIgnored public let settings: ImmichSettings
    @ObservationIgnored public private(set) var service: (any ImmichService)?
    @ObservationIgnored weak var app: AppModel?

    // Settings ▸ Immich
    public var serverEntry: String
    /// The API key being typed (never shown back once saved).
    public var keyEntry = ""
    public var pathOnServerEntry: String
    public var pathOnMacEntry: String
    public private(set) var hasKey: Bool
    public private(set) var busy = false
    /// The connection test's or the path detection's answer.
    public private(set) var message: String?
    public private(set) var libraries: [ImmichLibrary] = []

    // Albums
    public private(set) var syncing = false
    /// The last album sync, in words (Settings and the status bar).
    public private(set) var lastSync: String?
    public private(set) var lastSyncFailed = false

    // Developed copies
    public private(set) var rendering = false

    /// Quiet time after a change before out-of-date copies are rendered again (well
    /// within the archive's own quiet time, so the run carries them).
    @ObservationIgnored public var renderDelay: Duration = .seconds(10)
    /// How often to look for sessions the archive carried (their albums are synced).
    @ObservationIgnored public var pollInterval: Duration = .seconds(15)
    /// A sync that found photos Immich hasn't indexed yet tries again after this.
    @ObservationIgnored public var retryDelay: Duration = .seconds(120)
    @ObservationIgnored private var renderTask: Task<Void, Never>?
    @ObservationIgnored private var pollTask: Task<Void, Never>?
    @ObservationIgnored private var retryTask: Task<Void, Never>?
    @ObservationIgnored private var renderAgain = false

    public init(settings: ImmichSettings) {
        self.settings = settings
        serverEntry = settings.serverURL
        pathOnServerEntry = settings.pathOnServer
        pathOnMacEntry = settings.pathOnMac
        // The key is read (a Keychain prompt for a new build) only once Immich is in
        // use: `loadKeyState()` from the settings pane, or wiring with the integration on.
        hasKey = false
        lastSync = settings.lastSync
    }

    /// Whether a key is saved (reads the Keychain; never while Immich is off).
    public func loadKeyState() {
        guard isOn else { return }
        hasKey = settings.apiKey != nil
    }

    /// Wire the live service; the archive starts publishing when Immich is set up.
    public func configure(service: any ImmichService, app: AppModel, watch: Bool = true) {
        self.service = service
        self.app = app
        loadKeyState()
        Task { await applyPublish() }
        if watch, isOn { startWatching() }
    }

    /// Set up, wired, and turned on in Settings ▸ Plugins.
    public var isSetUp: Bool { isOn && service != nil && settings.isSetUp }

    /// The Immich integration is on (Settings ▸ Plugins; always, without an app).
    public var isOn: Bool { app?.features.isOn(.immich) ?? true }

    /// Settings ▸ Plugins turned Immich on or off. Off: the album watch, retries and
    /// developed-copy renders stop (a render or sync already going finishes), the archive
    /// stops writing sidecars and queueing album syncs, and the badges go. Nothing is
    /// deleted: the server, key, rules and copies stay. On: everything resumes.
    public func featureChanged(_ on: Bool) {
        Task { await applyPublish() }
        if on {
            loadKeyState()
            guard service != nil else { return }
            startWatching()
            sessionOpened()
        } else {
            stopWatching()
            apply([:])
        }
    }

    var server: ImmichServer? {
        guard let key = settings.apiKey, !settings.serverURL.isEmpty else { return nil }
        return ImmichServer(url: settings.serverURL, apiKey: key)
    }

    /// Tell the archive what to do on each run (sidecars, the album queue).
    func applyPublish() async {
        await service?.setPublish(isOn ? settings.publishConfig : nil)
    }

    // MARK: Settings

    public var canSave: Bool {
        serverEntry.trimmingCharacters(in: .whitespaces) != settings.serverURL || !keyEntry.isEmpty
            || pathOnServerEntry.trimmingCharacters(in: .whitespaces) != settings.pathOnServer
            || pathOnMacEntry.trimmingCharacters(in: .whitespaces) != settings.pathOnMac
    }

    /// Save the address and mapping (preferences) and the key, if typed (the Keychain).
    public func save() async {
        settings.serverURL = serverEntry
        settings.pathOnServer = pathOnServerEntry
        settings.pathOnMac = pathOnMacEntry
        serverEntry = settings.serverURL
        pathOnServerEntry = settings.pathOnServer
        pathOnMacEntry = settings.pathOnMac
        if !keyEntry.isEmpty {
            guard settings.apiKeyItem.write(keyEntry.trimmingCharacters(in: .whitespacesAndNewlines)) else {
                message = "The Keychain refused the API key."
                return
            }
            keyEntry = ""
            hasKey = true
        }
        message = "Saved."
        await applyPublish()
    }

    /// Forget the server's API key (the Keychain item).
    public func removeKey() async {
        guard settings.apiKeyItem.write(nil) else {
            message = "The API key couldn't be removed from your Keychain."
            return
        }
        hasKey = false
        message = "API key removed."
        await applyPublish()
    }

    /// Options and rules changed (toggles, steppers, the rule list).
    public func settingsChanged() {
        Task { await applyPublish() }
    }

    public func testConnection() async {
        guard let service, let server else {
            message = "Enter the server address and an API key, then Save."
            return
        }
        busy = true
        defer { busy = false }
        do {
            let c = try await service.testConnection(server: server)
            libraries = c.libraries
            let who = c.userName.isEmpty ? c.userEmail : c.userName
            var m = "Connected as \(who)\(c.isAdmin ? " (admin)" : "")."
            if let note = c.librariesNote { m += " " + note }
            message = m
        } catch {
            message = (error as? LocalizedError)?.errorDescription ?? "\(error)"
        }
    }

    /// Fill the path mapping from what Immich knows: an archived file it indexed, else
    /// a library whose import path is named like a folder of the archive's path.
    public func detectMapping() async {
        guard let service, let server, let archive = app?.settings.archiveTarget, !archive.isEmpty else {
            message = "Set the archive folder (Settings ▸ Archive) and the server first."
            return
        }
        busy = true
        defer { busy = false }
        let known = await Task.detached(priority: .utility) { Self.archivedFile(under: URL(fileURLWithPath: archive)) }.value
        do {
            guard let d = try await service.detectMapping(server: server, archiveRoot: archive, knownFile: known?.path) else {
                message = "Immich didn't tell where it sees the archive: enter both folders by hand."
                return
            }
            pathOnServerEntry = d.mapping.immichRoot
            pathOnMacEntry = d.mapping.localRoot
            message = d.how == "asset"
                ? "Found an archived photo in Immich: check the folders, then Save."
                : "Guessed from the library's import path: check the folders, then Save."
        } catch {
            message = (error as? LocalizedError)?.errorDescription ?? "\(error)"
        }
    }

    /// A photo the archive holds (a few levels down), to find in Immich.
    nonisolated static func archivedFile(under root: URL) -> URL? {
        let exts: Set<String> = ["arw", "cr2", "cr3", "nef", "raf", "orf", "rw2", "dng", "jpg", "jpeg", "heic"]
        guard let e = FileManager.default.enumerator(at: root, includingPropertiesForKeys: nil,
                                                     options: [.skipsHiddenFiles, .skipsPackageDescendants]) else { return nil }
        var seen = 0
        while let url = e.nextObject() as? URL {
            seen += 1
            if seen > 5000 { return nil }
            if e.level > 5 { e.skipDescendants() }
            if exts.contains(url.pathExtension.lowercased()) { return url }
        }
        return nil
    }

    // MARK: Developed copies

    /// The photos the commands work on: the selection's photos with a file (no videos).
    var targets: [PhotoID] {
        guard let app else { return [] }
        return app.selectedIDs.filter { app.photo($0).map { $0.url != nil && !$0.isVideo } ?? false }
    }

    public var canDevelop: Bool { isSetUp && app?.currentSession != nil && !targets.isEmpty }

    /// Selected photos that have a developed copy.
    public var canStopDeveloping: Bool {
        guard let app, service != nil, isOn else { return false }
        return targets.contains { app.item($0)?.immichCopy != nil }
    }

    public var developTitle: String {
        let n = targets.count
        return n > 1 ? "Keep Developed Copies for Immich (\(n) Photos)" : "Keep a Developed Copy for Immich"
    }

    public var stopTitle: String { "Stop Updating Developed Copy" }

    public var developHelp: String {
        if !isOn || !settings.isSetUp { return "Set up Immich in Settings ▸ Immich first" }
        return "A full-size JPEG with every edit, in the session's Developed folder; the archive carries it to Immich"
    }

    /// Photo ▸ Keep a Developed Copy for Immich: mark the selection and render the copies.
    public func developSelected() async {
        guard let app, let service, let session = app.currentSession, isOn else { return }
        guard settings.isSetUp else {
            app.show(Toast(ImmichError.notSetUp.errorDescription!, symbol: "photo.badge.arrow.down", isError: true))
            return
        }
        let ids = targets
        guard !ids.isEmpty else { return }
        do {
            _ = try await service.mark(ids, developed: true, in: session)
            app.show(Toast(ids.count == 1 ? "Making a developed copy for Immich…"
                : "Making developed copies of \(ids.count) photos for Immich…", symbol: "photo.badge.arrow.down"))
            await refresh(render: true)
        } catch {
            app.show(Toast((error as? LocalizedError)?.errorDescription ?? "\(error)",
                           symbol: "exclamationmark.triangle", isError: true))
        }
    }

    /// Photo ▸ Stop Updating Developed Copy: unmark the selection. The copies already
    /// made stay (in Developed/ and in the archive): nothing is deleted.
    public func stopDeveloping() async {
        guard let app, let service, let session = app.currentSession else { return }
        let ids = targets.filter { app.item($0)?.immichCopy != nil }
        guard !ids.isEmpty else { return }
        do {
            let r = try await service.mark(ids, developed: false, in: session)
            let kept = r.keptFiles.count
            app.show(Toast(kept == 0 ? "Stopped. No developed copy had been made yet."
                : "Stopped. The \(kept == 1 ? "developed copy stays" : "\(kept) developed copies stay") in Developed and "
                    + "in the archive; delete them yourself if you no longer want them in Immich.",
                symbol: "photo.badge.arrow.down"))
            await refresh(render: false)
        } catch {
            app.show(Toast((error as? LocalizedError)?.errorDescription ?? "\(error)",
                           symbol: "exclamationmark.triangle", isError: true))
        }
    }

    /// Photos changed (edits, ratings, keywords…): their copies are rendered again
    /// after a quiet time.
    public func photosChanged() {
        guard isSetUp else { return }
        renderTask?.cancel()
        let delay = renderDelay
        renderTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            await self?.refresh(render: true)
        }
    }

    /// Another session opened: show its copies, and bring them up to date.
    public func sessionOpened() {
        renderTask?.cancel()
        renderTask = Task { [weak self] in await self?.refresh(render: true) }
    }

    /// Read the copies' states into the browser (`PhotoItem.immichCopy`); with
    /// `render`, make the missing and out-of-date ones.
    public func refresh(render: Bool) async {
        guard let app else { return }
        guard isOn, let service, let session = app.currentSession, settings.isSetUp else {
            apply([:])
            return
        }
        let states = (try? await service.developed(in: session, options: settings.developOptions)) ?? []
        guard app.currentSession?.id == session.id else { return }
        apply(Dictionary(states.map { (PhotoID($0.variantId), ImmichCopyBadge($0.state)) }) { a, _ in a })
        if render, states.contains(where: { $0.state != .current }) { await renderPending() }
    }

    func apply(_ badges: [PhotoID: ImmichCopyBadge]) {
        guard let app else { return }
        for id in app.order where app.item(id)?.immichCopy != badges[id] {
            app.item(id)?.immichCopy = badges[id]
        }
    }

    /// Render the copies that are missing or out of date through the export (it has the
    /// AI masks), then put each in place.
    func renderPending() async {
        guard !rendering else {
            renderAgain = true
            return
        }
        guard let app, let service, let session = app.currentSession else { return }
        rendering = true
        defer { rendering = false }
        repeat {
            renderAgain = false
            guard isSetUp else { break } // turned off: the batch already rendered was placed
            guard let plan = try? await service.plan(in: session, options: settings.developOptions), !plan.jobs.isEmpty
            else { break }
            let staging = URL(fileURLWithPath: plan.stagingDir, isDirectory: true)
            var jobs: [PhotoID: ImmichDevelopJob] = [:]
            var items: [ExportJobItem] = []
            for job in plan.jobs {
                let id = PhotoID(job.variantId)
                guard let p = app.photo(id) else { continue }
                jobs[id] = job
                items.append(ExportJobItem(photoID: id, url: URL(fileURLWithPath: job.originalPath), seq: items.count + 1,
                                           edit: app.edit(for: id), keywords: p.keywords, metadata: p.metadata,
                                           location: p.location, rating: p.rating, colorTag: p.colorTag, pick: p.pick))
            }
            guard !items.isEmpty else { break }
            let longEdge = settings.longEdge
            let recipe = ExportRecipe(name: "Immich", format: .jpeg, quality: settings.quality,
                                      sizeMode: longEdge > 0 ? .longEdge : .original, sizeValue: Double(longEdge),
                                      fileNameTemplate: "{seq}", destination: staging, labelTags: settings.labelTags)
            let credit = ExportRecipeStore(defaults: app.settings.userDefaults).credit
            let outcome: ExportOutcome
            do {
                outcome = try await app.exporter.export(ExportJob(items: items, recipes: [recipe], defaultFolder: staging,
                                                                  artist: credit.artist,
                                                                  copyright: credit.copyright)) { _ in }
            } catch {
                app.show(Toast("Developed copies not made: \((error as? LocalizedError)?.errorDescription ?? "\(error)")",
                               symbol: "exclamationmark.triangle", isError: true))
                break
            }
            var placed = 0
            for w in outcome.written {
                guard let job = jobs[w.photoID] else { continue }
                do {
                    if try await service.rendered(job, staged: w.url, in: session) != nil { placed += 1 }
                } catch {
                    app.show(Toast((error as? LocalizedError)?.errorDescription ?? "\(error)",
                                   symbol: "exclamationmark.triangle", isError: true))
                }
            }
            if let f = outcome.failures.first {
                app.show(Toast("Developed copy not made: \(f)", symbol: "exclamationmark.triangle", isError: true))
            }
            if placed > 0 { await app.archive.noteChanges(placed, in: session) }
            await refresh(render: false)
        } while renderAgain
    }

    // MARK: Albums

    public func startWatching() {
        pollTask?.cancel()
        let interval = pollInterval
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: interval)
                guard let self, !Task.isCancelled else { return }
                if self.isSetUp, !self.syncing, await self.service?.pendingAlbumSyncs() ?? 0 > 0 {
                    await self.syncAlbums(every: false)
                }
            }
        }
    }

    public func stopWatching() {
        pollTask?.cancel()
        retryTask?.cancel()
        renderTask?.cancel()
    }

    public var canSync: Bool { isSetUp && !syncing }

    /// "Sync Albums Now" (`every`: every session the archive knows), and after each
    /// archive run (the sessions it carried).
    public func syncAlbums(every: Bool) async {
        guard let service, let server, isSetUp, !syncing else { return }
        syncing = true
        defer { syncing = false }
        do {
            let r = try await service.syncAlbums(server: server, mapping: settings.mapping,
                                                 rules: settings.rules.map(\.ffi), every: every)
            record(Self.summary(r), failed: r.error != nil)
            if r.missing > 0 || r.offline { scheduleRetry() }
        } catch is CancellationError {
        } catch {
            record("Immich: \((error as? LocalizedError)?.errorDescription ?? "\(error)")", failed: true)
        }
    }

    private func record(_ text: String, failed: Bool) {
        lastSync = text
        lastSyncFailed = failed
        settings.lastSync = text
    }

    /// Photos not indexed yet (Immich scans in the background): try again soon.
    private func scheduleRetry() {
        retryTask?.cancel()
        let delay = retryDelay
        retryTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            await self?.syncAlbums(every: true)
        }
    }

    /// Settings ▸ Immich while the archive goes to object storage (album sync is skipped).
    public nonisolated static let objectStorageNote =
        "The archive goes to object storage, which Immich can't index, so album sync is skipped. "
        + "To use Immich, choose a folder (the NAS Immich reads) in Settings ▸ Archive."

    /// "Immich: 3 added, 1 removed · 2 not in Immich yet".
    public static func summary(_ r: ImmichSyncReport) -> String {
        if r.sessions == 0, r.error == nil, let note = r.scanNote { return "Immich: \(note)" }
        if let e = r.error {
            return r.offline ? "Immich: server not reachable; albums sync when it is back"
                : r.keyRefused ? "Immich refused the API key: check it in Settings ▸ Immich" : "Immich: \(e)"
        }
        let added = r.albums.reduce(0) { $0 + Int($1.added) }
        let removed = r.albums.reduce(0) { $0 + Int($1.removed) }
        var parts = [added == 0 && removed == 0 ? "albums up to date" : "\(added) added, \(removed) removed"]
        if r.missing > 0 { parts.append("\(r.missing) not in Immich yet") }
        if !r.failures.isEmpty { parts.append("\(r.failures.count) problem\(r.failures.count == 1 ? "" : "s")") }
        let time = Date(timeIntervalSince1970: Double(r.finishedMs) / 1000).formatted(date: .omitted, time: .shortened)
        return "Immich: " + parts.joined(separator: " · ") + " (\(time))"
    }
}
