import LuxtableCore
import Foundation
import Observation

/// In-app updates from Luxtable's GitHub releases (docs/RELEASING.md): checks (automatic and
/// Check for Updates…), the update sheet, the verified download, and the install when the app
/// quits, reopening it where the user was (`ResumeState`) after Download & Restart.
///
/// The running bundle is never touched while the app runs: a verified copy waits in
/// `<support>/Updates/Ready`, and a small relauncher swaps it in once the app has exited,
/// keeping the old one as `<support>/Previous Version` (exactly one, replaced each time).
@MainActor @Observable
public final class Updater {
    public enum State: Equatable, Sendable {
        case idle
        case checking
        /// A manual check found nothing newer.
        case upToDate
        case available(UpdateRelease)
        case downloading(UpdateRelease, Double)
        case verifying(UpdateRelease)
        /// Verified and waiting: installed when the app quits.
        case ready(UpdateRelease)
        case failed(String)
        /// The app's folder isn't writable: the new app is shown in Finder instead.
        case manualInstall(UpdateRelease, URL)
    }

    /// "Updated to X" with its notes, once, after an update.
    public struct WhatsNew: Identifiable, Equatable, Sendable {
        public var version: String
        public var notes: [String]
        public var id: String { version }
    }

    /// The running app: its bundle, identifier and version.
    public struct Host: Sendable {
        public var bundle: URL
        public var identifier: String
        public var version: AppVersion
        public init(bundle: URL, identifier: String, version: AppVersion) {
            self.bundle = bundle
            self.identifier = identifier
            self.version = version
        }
    }

    public private(set) var state: State = .idle
    /// The update sheet is up (on the main window).
    public var sheetShown = false
    /// An automatic check found an update: the main window's small notice offers it.
    public private(set) var notice: UpdateRelease?
    public var whatsNew: WhatsNew?
    public private(set) var lastChecked: Date?
    /// Why the last automatic check failed (shown in Settings ▸ Updates only).
    public private(set) var lastError: String?
    public var automatic: Bool { didSet { defaults.set(automatic, forKey: Keys.automatic) } }
    public var includeBeta: Bool { didSet { defaults.set(includeBeta, forKey: Keys.beta) } }
    public private(set) var skipped: String? { didSet { defaults.set(skipped, forKey: Keys.skipped) } }

    /// Nil when the app can't update itself (not run from an app bundle, demo, snapshot).
    @ObservationIgnored public let host: Host?
    @ObservationIgnored let feed: any ReleaseFetching
    @ObservationIgnored let downloader: any ModelDownloader
    @ObservationIgnored let tools: UpdateTools
    /// `<support>/Updates`: downloads in flight and the verified update.
    @ObservationIgnored let root: URL
    /// Where the replaced app goes: `<support>/Previous Version/<name>.app`.
    @ObservationIgnored let backup: URL?
    @ObservationIgnored let defaults: UserDefaults
    @ObservationIgnored let now: @Sendable () -> Date
    /// Where the user is, for the next launch (the app sets it from `AppModel.resumeState`).
    @ObservationIgnored public var resumeState: @MainActor (_ version: String, _ notes: [String]) -> ResumeState = { v, n in
        var r = ResumeState()
        r.updatedTo = v
        r.notes = n
        return r
    }
    @ObservationIgnored let resumeStore: ResumeStore
    @ObservationIgnored private var running: Task<URL, Error>?
    @ObservationIgnored private var schedule: Task<Void, Never>?
    @ObservationIgnored private var restartWhenReady = false
    /// Restart (not a plain quit) asked for this termination.
    @ObservationIgnored private(set) var relaunchRequested = false

    nonisolated static let interval: TimeInterval = 24 * 3600
    nonisolated static let launchDelay: Duration = .seconds(10)
    nonisolated static let poll: Duration = .seconds(3600)

    enum Keys {
        static let automatic = "updates.automatic"
        static let beta = "updates.includeBeta"
        static let skipped = "updates.skippedVersion"
        static let lastCheck = "updates.lastCheck"
    }

    public init(host: Host?, defaults: UserDefaults, support: URL = AppPaths.support,
                feed: (any ReleaseFetching)? = nil, downloader: any ModelDownloader = URLSessionModelDownloader(),
                tools: UpdateTools = .live, resumeStore: ResumeStore? = nil,
                now: @escaping @Sendable () -> Date = { Date() }) {
        self.host = host
        self.defaults = defaults
        self.feed = feed ?? GitHubReleaseFetcher(version: host?.version.description ?? "0")
        self.downloader = downloader
        self.tools = tools
        self.now = now
        self.resumeStore = resumeStore ?? ResumeStore(url: support.appendingPathComponent("resume.json"))
        root = support.appendingPathComponent("Updates", isDirectory: true)
        backup = host.map { support.appendingPathComponent("Previous Version", isDirectory: true)
            .appendingPathComponent($0.bundle.lastPathComponent, isDirectory: true) }
        automatic = defaults.object(forKey: Keys.automatic) as? Bool ?? true
        includeBeta = defaults.bool(forKey: Keys.beta)
        skipped = defaults.string(forKey: Keys.skipped)
        lastChecked = defaults.object(forKey: Keys.lastCheck) as? Date
    }

    /// The app's updater: only when it runs from an app bundle in live mode (never `--demo`,
    /// `--snapshot`, `swift run` or tests).
    public static func live(model: AppModel) -> Updater {
        let main = Bundle.main
        var host: Host?
        if model.mode == .live, !CommandLine.arguments.contains("--snapshot"), main.bundleURL.pathExtension == "app",
           let id = main.bundleIdentifier,
           let v = (main.infoDictionary?["CFBundleShortVersionString"] as? String).flatMap(AppVersion.init) {
            host = Host(bundle: main.bundleURL, identifier: id, version: v)
        }
        let updater = Updater(host: host, defaults: model.settings.userDefaults)
        updater.resumeState = { [weak model] v, n in model?.resumeState(updatedTo: v, notes: n) ?? ResumeState() }
        return updater
    }

    public var isAvailable: Bool { host != nil }
    public var currentVersion: String { host?.version.description ?? "" }

    public var isBusy: Bool {
        switch state {
        case .checking, .downloading, .verifying: true
        default: false
        }
    }

    // MARK: - Launch

    /// At launch: the one-shot resume state (for `AppModel.pendingResume`). After an update to
    /// this version it also says what's new, once. Downloads a quit left behind are removed.
    public func launched() -> ResumeState? {
        guard let host else { return nil }
        try? FileManager.default.removeItem(at: root)
        let r = resumeStore.consume()
        if let r, r.updatedTo == host.version.description {
            whatsNew = WhatsNew(version: r.updatedTo ?? "", notes: r.notes)
        }
        return r
    }

    // MARK: - Checking

    /// Luxtable ▸ Check for Updates… and Settings' Check Now: the sheet shows the answer,
    /// "up to date" included, and a skipped version too.
    public func checkNow() {
        guard isAvailable else { return }
        sheetShown = true
        switch state {
        case .downloading, .verifying, .ready: return // the sheet shows it
        default: break
        }
        Task { await check(manual: true) }
    }

    func check(manual: Bool) async {
        guard let host, !isBusy else { return }
        if manual { state = .checking }
        do {
            let all = try ReleaseFeed.releases(from: try await feed.releases())
            lastChecked = now()
            defaults.set(lastChecked, forKey: Keys.lastCheck)
            lastError = nil
            if case .ready = state { return } // a verified update stays
            guard let r = ReleaseFeed.newest(all, after: host.version, beta: includeBeta) else {
                state = manual ? .upToDate : .idle
                notice = nil
                return
            }
            if !manual, r.version.description == skipped {
                if case .available = state { state = .idle }
                return
            }
            state = .available(r)
            if !manual { notice = r }
        } catch {
            let m = Self.message(error)
            if manual {
                state = .failed("Luxtable couldn’t check for updates: \(m)")
            } else {
                lastError = m
            }
        }
    }

    /// An automatic check is due: on, and the last one was a day ago or more (or never).
    public var isDue: Bool {
        guard automatic, isAvailable else { return false }
        guard let lastChecked else { return true }
        return now().timeIntervalSince(lastChecked) >= Self.interval
    }

    /// One automatic check if it is due and nothing else is going on.
    func tick() async {
        guard isDue else { return }
        switch state {
        case .idle, .upToDate, .failed: await check(manual: false)
        default: break
        }
    }

    /// Automatic checks: shortly after launch, then whenever one is due (looked at hourly).
    public func startAutomaticChecks(sleep: @escaping @Sendable (Duration) async throws -> Void = { try await Task.sleep(for: $0) }) {
        guard isAvailable, schedule == nil else { return }
        schedule = Task { [weak self] in
            do {
                try await sleep(Self.launchDelay)
                if let self, self.automatic { await self.check(manual: false) }
                while true {
                    try await sleep(Self.poll)
                    guard let self else { return }
                    await self.tick()
                }
            } catch {}
        }
    }

    public func stopAutomaticChecks() {
        schedule?.cancel()
        schedule = nil
    }

    // MARK: - The sheet's buttons

    /// The notice's View button.
    public func showSheet() {
        notice = nil
        sheetShown = true
    }

    /// Later: nothing changes; the next automatic check offers it again.
    public func later() {
        sheetShown = false
        notice = nil
        switch state {
        case .upToDate, .failed, .checking: state = .idle
        default: break
        }
    }

    /// Skip This Version: automatic checks stay quiet about it (a manual check still shows it).
    public func skip() {
        guard case .available(let r) = state else { return }
        skipped = r.version.description
        state = .idle
        notice = nil
        sheetShown = false
    }

    /// Download and verify; with `restart`, then quit and reopen on the new version. Without,
    /// it installs when the app quits.
    public func download(restart: Bool) async {
        guard case .available(let r) = state, let host else { return }
        restartWhenReady = restart
        state = .downloading(r, 0)
        let task = Task { try await self.fetch(r, host: host) }
        running = task
        defer { running = nil }
        do {
            _ = try await task.value
            state = .ready(r)
            if restartWhenReady { self.restart() }
        } catch {
            restartWhenReady = false
            if error is CancellationError || task.isCancelled {
                state = .available(r)
            } else {
                state = .failed(Self.message(error))
            }
        }
    }

    public func cancelDownload() {
        running?.cancel()
    }

    /// The verified app waiting to be installed.
    var readyApp: URL? {
        guard case .ready = state, let host else { return nil }
        return root.appendingPathComponent("Ready", isDirectory: true).appendingPathComponent(host.bundle.lastPathComponent)
    }

    /// Whether the app can replace its own bundle (its folder and itself writable).
    var canReplace: Bool {
        guard let host else { return false }
        let fm = FileManager.default
        return fm.isWritableFile(atPath: host.bundle.deletingLastPathComponent().path) && fm.isWritableFile(atPath: host.bundle.path)
    }

    /// Restart Now: quit the normal way (pending saves flush; a running export is asked
    /// about) and reopen on the new version where the user was. A folder Luxtable can't
    /// write to: the new app is shown in Finder to drag over by hand.
    public func restart() {
        guard case .ready(let r) = state, let app = readyApp else { return }
        restartWhenReady = false
        guard canReplace else {
            state = .manualInstall(r, app)
            tools.reveal(app)
            return
        }
        relaunchRequested = true
        tools.terminate()
    }

    /// The quit was cancelled (an alert's Cancel): nothing is installed yet; the update stays ready.
    public func quitCancelled() {
        relaunchRequested = false
    }

    /// The app is terminating for sure: with a verified update, keep where the user is and
    /// start the relauncher that swaps the bundles once this process has exited.
    public func applicationWillTerminate(pid: Int32 = ProcessInfo.processInfo.processIdentifier) {
        guard case .ready(let r) = state, let host, let app = readyApp, let backup, canReplace else { return }
        do {
            try resumeStore.save(resumeState(r.version.description, r.notes))
            try tools.spawn(UpdateTools.relauncher, [String(pid), host.bundle.path, app.path, backup.path,
                                                     relaunchRequested ? "1" : "0", "/usr/bin/open"])
        } catch {
            try? FileManager.default.removeItem(at: resumeStore.url) // no install: no "Updated to"
        }
    }

    // MARK: - Download and verify

    private func fetch(_ r: UpdateRelease, host: Host) async throws -> URL {
        let fm = FileManager.default
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let staging = root.appendingPathComponent(".staging-\(UUID().uuidString)", isDirectory: true)
        try fm.createDirectory(at: staging, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: staging) }
        let zip = staging.appendingPathComponent(r.asset.name)
        let total = Double(max(r.asset.size, 1))
        do {
            try await ModelFiles.fetch(r.asset.url, to: zip, sha256: r.asset.sha256, size: r.asset.size, path: r.asset.name,
                                       using: downloader, progress: { [weak self] bytes in
                let f = min(1, Double(bytes) / total)
                Task { @MainActor in if case .downloading(let r, _) = self?.state { self?.state = .downloading(r, f) } }
            })
        } catch let e as ModelFetchError {
            switch e {
            case .download(let m): throw UpdateError("The update couldn’t be downloaded: \(m)")
            case .checksum: throw UpdateError("The download isn’t the file GitHub published (checksum mismatch). Nothing was installed.")
            }
        }
        try Task.checkCancellation()
        state = .verifying(r)
        let app = try await Self.verify(zip: zip, in: staging, release: r, identifier: host.identifier, running: host.bundle,
                                        tools: tools)
        try Task.checkCancellation()
        let ready = root.appendingPathComponent("Ready", isDirectory: true)
        try? fm.removeItem(at: ready)
        try fm.createDirectory(at: ready, withIntermediateDirectories: true)
        let dest = ready.appendingPathComponent(host.bundle.lastPathComponent)
        try fm.moveItem(at: app, to: dest)
        return dest
    }

    /// Unpacks the checked zip and checks the app in it: exactly one app, this app's bundle
    /// identifier, the release's version, a valid signature by the signer of the `running`
    /// app (`CodeSigner.accepts`). Returns the app.
    nonisolated static func verify(zip: URL, in staging: URL, release r: UpdateRelease, identifier: String,
                                   running: URL, tools: UpdateTools) async throws -> URL {
        let fm = FileManager.default
        let size = (try? fm.attributesOfItem(atPath: zip.path)[.size] as? NSNumber)?.int64Value
        guard size == r.asset.size else { throw UpdateError("The download isn’t the expected size. Nothing was installed.") }
        let unpacked = staging.appendingPathComponent("unpacked", isDirectory: true)
        try await tools.unzip(zip, unpacked)
        let apps = ((try? fm.contentsOfDirectory(atPath: unpacked.path)) ?? []).filter { $0.hasSuffix(".app") }
        guard apps.count == 1, let name = apps.first else { throw UpdateError("The download doesn’t contain Luxtable.") }
        let app = unpacked.appendingPathComponent(name, isDirectory: true)
        let plist = (try? Data(contentsOf: app.appendingPathComponent("Contents/Info.plist")))
            .flatMap { try? PropertyListSerialization.propertyList(from: $0, format: nil) as? [String: Any] } ?? [:]
        let id = plist["CFBundleIdentifier"] as? String ?? ""
        guard id == identifier else { throw UpdateError("The download isn’t Luxtable (its identifier is “\(id)”). Nothing was installed.") }
        let version = plist["CFBundleShortVersionString"] as? String ?? ""
        guard version == r.version.description else {
            throw UpdateError("The download is version “\(version)”, not \(r.version). Nothing was installed.")
        }
        try await tools.verifySignature(app)
        guard try tools.signer(running).accepts(try tools.signer(app)) else {
            throw UpdateError("The download isn’t signed the way this copy of Luxtable is. Nothing was installed.")
        }
        return app
    }

    static func message(_ error: Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }
}

/// What Settings ▸ Updates and the update sheet say.
public enum UpdateText {
    public static func available(_ r: UpdateRelease, current: String) -> String {
        "Luxtable \(r.version) is available (you have \(current))."
    }

    public static func title(_ state: Updater.State) -> String {
        switch state {
        case .idle, .checking: "Checking for updates…"
        case .upToDate: "You’re up to date"
        case .available(let r): "Luxtable \(r.version) is available"
        case .downloading(let r, _): "Downloading Luxtable \(r.version)…"
        case .verifying(let r): "Checking Luxtable \(r.version)…"
        case .ready(let r): "Luxtable \(r.version) is ready to install"
        case .failed: "The update didn’t work"
        case .manualInstall(let r, _): "Install Luxtable \(r.version) by hand"
        }
    }

    public static func detail(_ state: Updater.State, current: String, beta: Bool) -> String {
        switch state {
        case .idle, .checking: ""
        case .upToDate:
            "Luxtable \(current) is the newest \(beta ? "version, betas included" : "version")."
        case .available: "You have \(current)."
        case .downloading(_, let f): "\(Int((f * 100).rounded()))%"
        case .verifying: "Checking its fingerprint and signature."
        case .ready: "It installs when you quit Luxtable. Restart now to use it right away: you’ll be back on the same photo."
        case .failed(let m): m
        case .manualInstall(_, let app):
            "Luxtable can’t replace itself in “\(app.deletingLastPathComponent().path)” (no permission there). "
                + "The new version is shown in Finder: quit Luxtable and drag it to your Applications folder."
        }
    }

    /// Settings ▸ Updates' status line.
    @MainActor public static func status(_ u: Updater) -> String {
        if !u.isAvailable { return "Updates are available only in the installed app." }
        switch u.state {
        case .checking: return "Checking…"
        case .available(let r): return "Luxtable \(r.version) is available."
        case .downloading(let r, let f): return "Downloading Luxtable \(r.version)… \(Int((f * 100).rounded()))%"
        case .verifying(let r): return "Checking Luxtable \(r.version)…"
        case .ready(let r): return "Luxtable \(r.version) installs when you quit."
        case .failed(let m): return m
        case .manualInstall(let r, _): return "Luxtable \(r.version) is waiting to be installed by hand."
        case .idle, .upToDate:
            if let e = u.lastError { return "The last check didn’t work: \(e)" }
            guard let d = u.lastChecked else { return "Not checked yet." }
            return "Last checked \(d.formatted(date: .abbreviated, time: .shortened))."
        }
    }
}
