import Foundation
import Observation

/// Stores that can make an empty session (File ▸ New Session…).
public protocol SessionCreating: SessionStore {
    /// Where `createSession(named:)` puts a new session (the app's sessions folder).
    func newSessionLocation() async -> URL
    /// An empty session `<folder>/<name>` (New Session… with a location chosen).
    func createSession(named name: String, in folder: URL) async throws -> SessionInfo
}

/// Stores that hold an open session (a Rust handle, its index connection and
/// background identity pass) until it is closed.
public protocol SessionClosing: SessionStore {
    /// Release `session`: its handle and background work go; nothing on disk changes.
    func close(_ session: SessionInfo) async
}

/// Stores that resolve a bare photo id in the session loaded last first: an open that a
/// newer one superseded after loading points the store back at the session shown.
public protocol CurrentSessionPointing: SessionStore {
    /// Resolve bare photo ids in `session` first (if it's loaded).
    func makeCurrent(_ session: SessionInfo) async
}

/// What a new session may be called. Mirrors ds-session's rule (no `/`, `:` or NUL, not
/// starting with a period, at most 200 bytes) and refuses a name already taken.
public enum SessionNameRules {
    public static let maxBytes = 200

    /// Why `name` can't be used, or nil. `taken` holds lower-cased names (sessions and
    /// folders already there; the default volume ignores case).
    public static func problem(_ name: String, taken: Set<String>) -> String? {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return "Enter a name for the session." }
        if trimmed.hasPrefix(".") { return "A session name can’t start with a period." }
        let illegal = ["/", ":"].filter(trimmed.contains)
        if !illegal.isEmpty {
            return "A session name can’t contain " + illegal.map { "“\($0)”" }.joined(separator: " or ") + "."
        }
        if trimmed.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) {
            return "A session name can’t contain control characters."
        }
        if trimmed.utf8.count > maxBytes { return "That name is too long." }
        if taken.contains(trimmed.lowercased()) { return "A session named “\(trimmed)” already exists here." }
        return nil
    }

    /// "Session 2026-10-03", or "Session 2026-10-03 2", "… 3" when that is taken.
    public static func defaultName(on date: Date = Date(), calendar: Calendar = .current, taken: Set<String>) -> String {
        let d = calendar.dateComponents([.year, .month, .day], from: date)
        let base = String(format: "Session %04d-%02d-%02d", d.year ?? 0, d.month ?? 0, d.day ?? 0)
        var name = base
        var n = 2
        while taken.contains(name.lowercased()) {
            name = "\(base) \(n)"
            n += 1
        }
        return name
    }
}

/// The New Session sheet: a name (checked as you type) and where it will be created.
@MainActor @Observable
public final class NewSessionModel {
    /// The name as typed (used trimmed).
    public var name: String {
        didSet { if name != oldValue { failure = nil } }
    }
    /// Where the session will be created (once known); "Choose…" changes it.
    public private(set) var location: URL?
    public private(set) var isCreating = false
    /// What the store said when creating failed (shown until the name changes).
    public private(set) var failure: String?
    /// Why the location can't take a session (inside another one), or nil.
    public private(set) var locationProblem: String?
    /// A caution about the location (a cloud-synced folder), or nil.
    public private(set) var locationWarning: String?
    /// Names in the session list (lower-cased); `taken` adds the location's folders.
    @ObservationIgnored private let listed: Set<String>
    @ObservationIgnored private var taken: Set<String>
    @ObservationIgnored private var suggested: String
    @ObservationIgnored private let now: Date
    @ObservationIgnored private weak var app: AppModel?

    public init(app: AppModel, now: Date = Date()) {
        self.app = app
        self.now = now
        listed = Set(app.sessionList.map { $0.name.lowercased() })
        taken = listed
        suggested = SessionNameRules.defaultName(on: now, taken: taken)
        name = suggested
    }

    /// The inline error under the name field, if any.
    public var problem: String? { failure ?? locationProblem ?? SessionNameRules.problem(name, taken: taken) }
    public var canCreate: Bool { !isCreating && problem == nil }

    /// Find where sessions go and which names are already used there.
    public func load() async {
        guard let store = app?.sessions as? any SessionCreating else { return }
        await use(await store.newSessionLocation())
    }

    /// "Choose…": create the session in `folder` instead.
    public func choose(_ folder: URL) async {
        guard app?.sessions is any SessionCreating else { return }
        failure = nil
        await use(folder)
    }

    /// Make `dir` the location; the names of its folders are taken (read off the main
    /// thread: the folder may be on a slow drive).
    private func use(_ dir: URL) async {
        location = dir
        let sessions = app?.sessionList ?? []
        let (names, why) = await Task.detached {
            ((try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? [], SessionLocation.problem(dir, sessions: sessions))
        }.value
        guard location == dir else { return } // another folder was chosen meanwhile
        locationProblem = why
        locationWarning = SessionLocation.cloudWarning(dir)
        taken = listed.union(names.map { $0.lowercased() })
        // The suggestion is taken there: suggest the next free one.
        if name == suggested, taken.contains(name.lowercased()) {
            suggested = SessionNameRules.defaultName(on: now, taken: taken)
            name = suggested
        }
    }

    /// Create the session and open it. Returns whether it worked (the sheet closes).
    @discardableResult
    public func create() async -> Bool {
        guard canCreate, let app else { return false }
        isCreating = true
        defer { isCreating = false }
        do {
            try await app.createSession(named: name.trimmingCharacters(in: .whitespacesAndNewlines), in: location)
            return true
        } catch {
            failure = error.localizedDescription
            return false
        }
    }
}

extension AppSettings {
    static let sessionClosedKey = "sessionClosedByUser"

    /// The user closed the session they were in: the next launch opens none. Cleared
    /// as soon as a session opens (launching normally reopens the last one).
    public var sessionClosed: Bool {
        get { userDefaults.bool(forKey: Self.sessionClosedKey) }
        set { userDefaults.set(newValue, forKey: Self.sessionClosedKey) }
    }
}

extension AppModel {
    /// The store can make empty sessions.
    public var canCreateSession: Bool { sessions is any SessionCreating }

    /// A session is open, nothing is moving its files, no sheet is up (it would be
    /// replaced) and no close is already running.
    public var canCloseSession: Bool {
        currentSession != nil && !isLoading && !cull.busy && sheet == nil && closing == nil
    }

    /// `url`'s path with the home folder shown as `~`.
    public nonisolated static func displayPath(_ url: URL, home: String = NSHomeDirectory()) -> String {
        let path = url.standardizedFileURL.path
        return path == home || path.hasPrefix(home + "/") ? "~" + path.dropFirst(home.count) : path
    }

    /// File ▸ New Session…
    public func beginNewSession() {
        guard canCreateSession, sheet == nil else { return } // never over another sheet (an export)
        sheet = .newSession
    }

    /// Make an empty session named `name` in `folder` (where the store keeps sessions
    /// when nil) and open it (it is then the selected session). The open session is
    /// saved first, as when switching.
    @discardableResult
    public func createSession(named name: String, in folder: URL? = nil) async throws -> SessionInfo {
        let s = if let folder, let store = sessions as? any SessionCreating {
            try await store.createSession(named: name, in: folder)
        } else {
            try await sessions.createSession(named: name)
        }
        await reloadSessions(select: s)
        if currentSession?.id != s.id { await open(s) }
        return s
    }

    /// File ▸ Close Session: back to the no-session state, deleting nothing.
    ///
    /// While imports, an export or an archive run are going it asks first (as quitting
    /// does); they keep running if the user closes anyway: they hold their own handles
    /// and files and finish safely on their own. Pending ratings and edits are written
    /// next; if they can't be, the session stays open and says why. Then the session's
    /// own background work stops (renders and prefetches, the preset hover preview, AI
    /// Cull, queued AI Denoise nobody else waits for, smart-folder refreshes) and the
    /// store releases its handle. The session stays in the list, and the next launch
    /// opens none.
    public func closeSession(imports: ImportRegistry = .shared, timeout: Duration = .seconds(5)) async {
        guard canCloseSession, let session = currentSession else { return } // a second ⌥⌘W is ignored
        let task = Task { await performClose(session, imports: imports, timeout: timeout) }
        closing = task
        await task.value
        closing = nil
    }

    private func performClose(_ session: SessionInfo, imports: ImportRegistry, timeout: Duration) async {
        if let alert = SessionCloseAlert.make(session: session.name, imports: imports.running.count,
                                              exporting: exportModel?.isRunning == true,
                                              archiving: Self.isArchiving(archiveStatus.state)),
           let confirmClose {
            guard await confirmClose(alert), currentSession?.id == session.id else { return }
        }
        viewer.canvasTool = .pan // a canvas tool commits what it holds, so it is saved too
        // Write until nothing is pending: a rating made while a write runs is written too.
        let deadline = ContinuousClock.now + timeout
        repeat {
            let left = deadline - .now
            guard left > .zero, await flushSaves(within: left) else {
                show(Toast("“\(session.name)” is still open: saving is taking too long. Try again in a moment.",
                           symbol: "exclamationmark.triangle", isError: true))
                return
            }
            if case .failed(let message, let pending) = saver.state {
                show(Toast("“\(session.name)” is still open: \(pending) change\(pending == 1 ? "" : "s") couldn’t be saved (\(message))",
                           symbol: "exclamationmark.triangle", isError: true))
                return
            }
        } while saver.hasUnsaved
        // Nothing pending and no suspension from here to the cleared state: no edit can
        // be made in between and lost.
        guard currentSession?.id == session.id else { return }
        viewer.sessionClosed()
        let urls = Set(photos.compactMap(\.url))
        denoiser?.cancelUnwaited(urls)
        upscaler?.cancel() // its new photos would have no session to go to
        if survey.isActive { survey.finish() }
        cull.sessionClosed()
        smartFolders.sessionClosed()
        commandBar.dismiss()
        clearSessionState()
        settings.sessionClosed = true
        // The final archive run takes its own reference to the handle before it goes.
        // An open meanwhile waits for this (`closing`), so it never loses its handle.
        let (archive, store) = (archive, sessions as? any SessionClosing)
        let tail = Task {
            await archive.sessionClosed(session)
            await store?.close(session)
        }
        releasing[session.id] = tail
        Task { [weak self] in
            await tail.value
            if self?.releasing[session.id] == tail { self?.releasing[session.id] = nil }
        }
        if await !Bounded.wait(closeTailLimit, { await tail.value }) {
            AppLog.session.error("closing \(session.name, privacy: .private) is still releasing its store")
        }
        show(Toast(Self.closedMessage(session.name, imports: imports.running.count,
                                      exporting: exportModel?.isRunning == true,
                                      archiving: Self.isArchiving(archiveStatus.state)),
                   symbol: "xmark.rectangle"))
    }

    /// An import finished: list its session and open it, unless the user closed their
    /// session meanwhile (closed stays closed, as the close said): then a toast offers it.
    public func importFinished(_ session: SessionInfo) async {
        guard currentSession == nil, settings.sessionClosed else {
            await reloadSessions(select: session)
            return
        }
        await reloadSessions()
        show(Toast("Import into “\(session.name)” finished", symbol: "sdcard", openSession: session))
    }

    /// The toast's Open button.
    public func openToastSession() async {
        guard let s = toast?.openSession else { return }
        toast = nil
        await open(sessionList.first { $0.id == s.id } ?? s)
    }

    /// An archive run is copying.
    nonisolated static func isArchiving(_ state: ArchiveStatus.State) -> Bool {
        if case .syncing = state { true } else { false }
    }

    /// What closing says: imports, exports and archive runs in flight are not stopped.
    nonisolated static func closedMessage(_ name: String, imports: Int, exporting: Bool, archiving: Bool = false) -> String {
        let running = SessionCloseAlert.running(imports: imports, exporting: exporting, archiving: archiving)
        guard !running.isEmpty else { return "Closed “\(name)”" }
        return "Closed “\(name)” (\(running.list) \(running.plural ? "keep" : "keeps") running)"
    }
}

/// Close Session while imports, an export or an archive run are going: what to ask.
public struct SessionCloseAlert: Equatable, Sendable {
    public var title: String
    public var message: String
    /// The confirming button (Cancel is the default).
    public static let confirm = "Close Session"

    /// nil: nothing is running, close without asking.
    public static func make(session: String, imports: Int, exporting: Bool, archiving: Bool) -> SessionCloseAlert? {
        let r = running(imports: imports, exporting: exporting, archiving: archiving)
        guard !r.isEmpty else { return nil }
        return SessionCloseAlert(
            title: "\(r.list.prefix(1).uppercased() + r.list.dropFirst()) \(r.plural ? "are" : "is") still running",
            message: "Close “\(session)” anyway? \(r.plural ? "They keep" : "It keeps") running and \(r.plural ? "finish" : "finishes") "
                + "on \(r.plural ? "their" : "its") own. Nothing is deleted, and the session stays in the Sessions list.")
    }

    /// "the import", "2 imports", "the export", "the archive run", joined in words.
    static func running(imports: Int, exporting: Bool, archiving: Bool) -> (list: String, plural: Bool, isEmpty: Bool) {
        var r: [String] = []
        if imports > 0 { r.append(imports == 1 ? "the import" : "\(imports) imports") }
        if exporting { r.append("the export") }
        if archiving { r.append("the archive run") }
        let list = r.count > 2 ? r.dropLast().joined(separator: ", ") + " and " + r[r.count - 1] : r.joined(separator: " and ")
        return (list, r.count > 1 || imports > 1, r.isEmpty)
    }
}

/// Where a new session may go (blocking file checks: call off the main thread).
public enum SessionLocation {
    /// Why a session can't be created in `dir`, or nil: it is (or is inside) a listed
    /// session or photo folder, or a folder Darkslide keeps a session in (`.ds/`).
    public static func problem(_ dir: URL, sessions: [SessionInfo]) -> String? {
        let d = canonical(dir)
        for s in sessions {
            let root = canonical(s.url)
            if d.path == root.path || d.path.hasPrefix(root.path + "/") {
                return "That folder is inside “\(s.name)”. Choose a folder outside any session."
            }
        }
        var probe = d
        while probe.path != "/" {
            if FileManager.default.fileExists(atPath: probe.appendingPathComponent(".ds").path) {
                return "That folder is inside the session “\(probe.lastPathComponent)”. Choose a folder outside any session."
            }
            probe.deleteLastPathComponent()
        }
        return nil
    }

    /// A caution for folders a sync service manages (iCloud Drive, Dropbox, …): files
    /// may be evicted or synced mid-write.
    public static func cloudWarning(_ dir: URL) -> String? {
        let p = dir.standardizedFileURL.path
        let synced = ["/Library/Mobile Documents/", "/Library/CloudStorage/", "/Dropbox/", "/Google Drive/", "/OneDrive/"]
        guard synced.contains(where: { p.contains($0) || p.hasSuffix(String($0.dropLast())) }) else { return nil }
        return "This folder is synced by a cloud service. Originals may be offloaded or synced while Darkslide writes; "
            + "a local or external drive is safer."
    }

    /// `dir` and those of its parents that don't exist yet, deepest first (what creating
    /// it makes, and so what a failed create may remove).
    public static func missing(_ dir: URL) -> [URL] {
        var out: [URL] = []
        var u = dir.standardizedFileURL
        while u.path != "/", !FileManager.default.fileExists(atPath: u.path) {
            out.append(u)
            u.deleteLastPathComponent()
        }
        return out
    }

    /// After a failed create: remove what `missing` listed. The session folder itself
    /// (the first) goes only if it holds nothing but the empty scaffold a create makes
    /// (`Capture`, `Selects`, `Output`, `Trash`, `.ds`): a create that failed because
    /// someone else made the folder first must not delete theirs. Each parent goes only
    /// while it is empty.
    public static func cleanUp(_ made: [URL]) {
        guard let dir = made.first else { return }
        guard holdsOnlyScaffold(dir) else { return }
        try? FileManager.default.removeItem(at: dir)
        for parent in made.dropFirst() {
            guard (try? FileManager.default.contentsOfDirectory(atPath: parent.path))?.isEmpty == true else { return }
            try? FileManager.default.removeItem(at: parent)
        }
    }

    /// Whether `dir` is a just-created, empty session: only the scaffold folders (empty
    /// but for subfolders) and the `.ds` data folder.
    static func holdsOnlyScaffold(_ dir: URL) -> Bool {
        let fm = FileManager.default
        guard let top = try? fm.contentsOfDirectory(atPath: dir.path) else { return true } // nothing there
        for name in top {
            if name == ".ds" { continue }
            guard ["Capture", "Selects", "Output", "Trash"].contains(name) else { return false }
            guard let walk = fm.enumerator(at: dir.appendingPathComponent(name), includingPropertiesForKeys: [.isDirectoryKey])
            else { return false }
            for case let url as URL in walk where (try? url.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory != true {
                return false
            }
        }
        return true
    }

    static func canonical(_ url: URL) -> URL { url.standardizedFileURL.resolvingSymlinksInPath() }
}

/// Waiting on work that may never finish (a hung network volume).
enum Bounded {
    /// Run `work` and wait at most `limit`; whether it finished (it goes on regardless).
    static func wait(_ limit: Duration, _ work: @escaping @Sendable () async -> Void) async -> Bool {
        await withCheckedContinuation { cont in
            let once = FirstResult()
            Task {
                await work()
                if once.claim() { cont.resume(returning: true) }
            }
            Task {
                try? await Task.sleep(for: limit)
                if once.claim() { cont.resume(returning: false) }
            }
        }
    }
}
