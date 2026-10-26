import Foundation

/// Where the user was when Luxtable quit to install an update: the next launch opens the
/// same session on the same photo, in the same view, instead of the session's first photo.
/// One-shot: `ResumeStore.consume()` reads and deletes it.
public struct ResumeState: Codable, Equatable, Sendable {
    /// The filter bar's criteria (not a smart folder, person or map place: those are per run).
    public struct Filter: Codable, Equatable, Sendable {
        public var minRating = 0
        public var unratedOnly = false
        public var colorTag: Int?
        public var pick = PhotoFilter.PickFilter.all.rawValue
        public var text = ""
    }

    public var sessionID: UUID?
    /// The session's folder (its id is derived from it; the path is the fallback).
    public var sessionPath: String?
    /// The focused photo: its variant id (kept in its sidecar), its original's path and file
    /// name (fallbacks when the index was rebuilt and ids moved).
    public var focusedID: Int64?
    public var focusedPath: String?
    public var focusedName: String?
    public var selection: [Int64] = []
    public var browserMode: String?
    public var toolTab: String?
    public var fullScreen = false
    public var panelsHidden = false
    public var filmstripHidden = false
    public var filter: Filter?
    /// The version being installed and its short release notes ("Updated to X" at the next launch).
    public var updatedTo: String?
    public var notes: [String] = []

    public init() {}
}

/// `<support>/resume.json`, written atomically, read once.
public struct ResumeStore: Sendable {
    public let url: URL

    public init(url: URL = AppPaths.support.appendingPathComponent("resume.json")) { self.url = url }

    public func save(_ state: ResumeState) throws {
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try JSONEncoder().encode(state).write(to: url, options: .atomic)
    }

    /// The saved state, deleted as it is read (a crash on the way never replays it). Unreadable: nil.
    public func consume() -> ResumeState? {
        guard let data = try? Data(contentsOf: url) else { return nil }
        try? FileManager.default.removeItem(at: url)
        return try? JSONDecoder().decode(ResumeState.self, from: data)
    }
}

extension AppModel {
    /// What `ResumeState` keeps of the window as it is now (nothing of a session if none is open).
    public func resumeState(updatedTo version: String? = nil, notes: [String] = []) -> ResumeState {
        var r = ResumeState()
        r.updatedTo = version
        r.notes = notes
        guard let session = currentSession else {
            if browserMode == .library { r.browserMode = browserMode.rawValue } // [library] needs no session
            return r
        }
        r.sessionID = session.id
        r.sessionPath = session.url.path
        if let p = focusedPhoto {
            r.focusedID = p.id.raw
            r.focusedPath = p.url?.path
            r.focusedName = p.filename
        }
        r.selection = selection.map(\.raw).sorted()
        r.browserMode = browserMode.rawValue
        r.toolTab = toolTab.rawValue
        r.fullScreen = viewer.state.fullScreen
        r.panelsHidden = panelsHidden
        r.filmstripHidden = filmstripHidden
        r.filter = ResumeState.Filter(minRating: filter.minRating, unratedOnly: filter.unratedOnly,
                                      colorTag: filter.colorTag?.rawValue, pick: filter.pick.rawValue, text: filter.text)
        return r
    }

    /// [library] Whether `start()` shows the Library and leaves the session closed: after an
    /// update, a session the user had open wins (it reopens where they were), and the Library
    /// they were in without one is kept; otherwise the Open in setting decides.
    func startsInLibrary() -> Bool {
        if let r = pendingResume {
            if r.sessionID != nil || r.sessionPath != nil { return false }
            if r.browserMode == BrowserMode.library.rawValue { return true }
        }
        return settings.startView == .library
    }

    /// [library] Leaving the Library opens the session a Library start left closed.
    func openDeferredSession() {
        guard let s = deferredSession else { return }
        deferredSession = nil
        guard currentSession == nil else { return }
        Task { await open(s) }
    }

    /// The session `start()` reopens: the resumed one when it is listed and can open,
    /// otherwise the most recent.
    func sessionToReopen() -> SessionInfo? {
        if let r = pendingResume,
           let s = sessionList.first(where: { $0.id == r.sessionID })
           ?? sessionList.first(where: { r.sessionPath != nil && $0.url.path == r.sessionPath }),
           s.unavailable == nil {
            return s
        }
        pendingResume = nil // its session is gone: the usual launch
        return sessionList.first
    }

    /// After `open(_:)` loaded `session`: focus the photo the user was on before an update
    /// (once), or the first one.
    func focusAfterOpen(_ session: SessionInfo) {
        guard let r = pendingResume,
              r.sessionID == session.id || (r.sessionPath != nil && r.sessionPath == session.url.path) else {
            focus(visibleIDs.first, extend: false)
            return
        }
        pendingResume = nil
        if let m = r.browserMode.flatMap(BrowserMode.init(rawValue:)) { browserMode = m }
        if let t = r.toolTab.flatMap(ToolTab.init(rawValue:)) { toolTab = t }
        panelsHidden = r.panelsHidden
        filmstripHidden = r.filmstripHidden
        if let f = r.filter {
            var restored = PhotoFilter(minRating: f.minRating, colorTag: f.colorTag.flatMap(ColorTag.init(rawValue:)),
                                       pick: PhotoFilter.PickFilter(rawValue: f.pick) ?? .all, text: f.text,
                                       unratedOnly: f.unratedOnly)
            restored.missingOnly = filter.missingOnly
            filter = restored
        }
        let target = Self.resumedPhoto(r, in: photos)
        // A filter that now hides it gives way: the photo matters more than the filter.
        if let target, visibleIndex(target) == nil { filter = PhotoFilter() }
        guard let target, visibleIndex(target) != nil else {
            focus(visibleIDs.first, extend: false)
            viewer.state.fullScreen = r.fullScreen
            return
        }
        focus(target, extend: false)
        let kept = Set(r.selection.map(PhotoID.init)).filter { visibleIndex($0) != nil }
        if kept.count > 1, kept.contains(target) { selection = kept }
        viewer.state.fullScreen = r.fullScreen
    }

    /// The photo `r` was on: by id when it is still the same file, else by the original's
    /// path, else by file name. Nil: it isn't in the session any more.
    nonisolated static func resumedPhoto(_ r: ResumeState, in photos: [Photo]) -> PhotoID? {
        if let id = r.focusedID, let p = photos.first(where: { $0.id.raw == id }),
           r.focusedPath == nil || p.url?.path == r.focusedPath,
           r.focusedName == nil || p.filename == r.focusedName {
            return p.id
        }
        if let path = r.focusedPath, let p = photos.first(where: { $0.url?.path == path }) { return p.id }
        if let name = r.focusedName, let p = photos.first(where: { $0.filename == name }) { return p.id }
        return nil
    }
}
