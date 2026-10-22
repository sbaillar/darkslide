import Foundation

/// Stores whose session list is the recent-folders list (live mode, the folder
/// store): Sessions ▸ "Remove from List" forgets an entry (L12). The folder
/// and its session data are never touched.
public protocol RecentSessionsEditing: SessionStore {
    func removeFromRecents(_ session: SessionInfo) async
    /// Clear Menu: forget every recent but `sessions` (the open one, unfinished imports).
    func clearRecents(keeping sessions: [SessionInfo]) async
    /// `session` was opened now (its "Opened …" date).
    func noteOpened(_ session: SessionInfo) async
}

extension AppModel {
    /// "Remove from List" is offered for other sessions than the open one.
    public func canRemoveFromList(_ s: SessionInfo) -> Bool {
        sessions is any RecentSessionsEditing && currentSession?.id != s.id
    }

    public func removeFromList(_ s: SessionInfo) async {
        guard canRemoveFromList(s), let store = sessions as? any RecentSessionsEditing else { return }
        await store.removeFromRecents(s)
        await search.sessionForgotten(s) // [search] not offered in All Sessions results either
        await reloadSessions()
    }

    /// Sessions-list subtitle: photo count and where the folder is (`~` for home).
    public static func sessionSubtitle(_ s: SessionInfo, home: String = NSHomeDirectory()) -> String {
        var parent = s.url.deletingLastPathComponent().path
        if parent == home || parent.hasPrefix(home + "/") { parent = "~" + parent.dropFirst(home.count) }
        return "\(PanelChrome.photoCount(s.photoCount)) · \(parent)"
    }
}

// MARK: - Open Recent

/// A recent session as File ▸ Open Recent, the Open sheet and the session menu list it.
public struct RecentEntry: Identifiable, Equatable, Sendable {
    public let session: SessionInfo
    /// The open session.
    public let isCurrent: Bool

    public var id: UUID { session.id }
    public var name: String { session.name }
    /// Where the folder is (`~` for home).
    public var path: String { AppModel.displayPath(session.url) }
    public var lastOpened: Date? { session.lastOpened }

    /// Why it isn't usable as it is ("Drive not connected"), nil when it is.
    public var reason: String? {
        session.unavailable.map { $0.label.prefix(1).uppercased() + $0.label.dropFirst() }
    }

    /// The menu item: the name, then why it can't open as it is (choosing it then
    /// offers Locate: the drive may be mounted under another name).
    public var menuTitle: String { reason.map { "\(name) (\($0))" } ?? name }

    /// "Opened 2 days ago" (nil: not known).
    public func openedText(now: Date = Date()) -> String? {
        guard let lastOpened else { return nil }
        let f = RelativeDateTimeFormatter()
        f.dateTimeStyle = .named
        return "Opened " + f.localizedString(for: lastOpened, relativeTo: now)
    }
}

extension AppModel {
    /// Recents can be opened: no sheet is up (opening would replace it) and no close runs.
    public var canOpenRecent: Bool { sheet == nil && closing == nil }

    /// How many recents File ▸ Open Recent, the Open sheet and the session menu list.
    public static let recentLimit = 10

    /// Recent sessions, newest first (the session list is the recents list).
    public var recentEntries: [RecentEntry] {
        sessionList.prefix(Self.recentLimit).map { RecentEntry(session: $0, isCurrent: $0.id == currentSession?.id) }
    }

    /// Clear Menu: there is something besides the open session to forget.
    public var canClearRecents: Bool {
        sessions is any RecentSessionsEditing && sessionList.contains { $0.id != currentSession?.id }
    }

    /// File ▸ Open Recent ▸ Clear Menu: forget every recent but the open session and
    /// sessions with an unfinished import (so it is still offered to resume). Only the
    /// list changes; no folder or session data is touched.
    public func clearRecents() async {
        guard canClearRecents, let store = sessions as? any RecentSessionsEditing else { return }
        var keep = currentSession.map { [$0] } ?? []
        if let resuming = importer as? any ImportResuming {
            let pending = Set(await resuming.pendingImports(including: sessionList.map(\.url))
                .map { $0.sessionURL.standardizedFileURL.path })
            keep += sessionList.filter { $0.id != currentSession?.id && pending.contains($0.url.standardizedFileURL.path) }
        }
        await store.clearRecents(keeping: keep)
        await reloadSessions()
    }

    /// Stamp `session`'s "Opened …" date (recents stores only).
    func noteOpened(_ session: SessionInfo) async {
        guard let store = sessions as? any RecentSessionsEditing else { return }
        await store.noteOpened(session)
        if let i = sessionList.firstIndex(where: { $0.id == session.id }) { sessionList[i].lastOpened = Date() }
    }

    /// File ▸ Open…: the Open sheet (recents, then Choose Folder…) when there are recents.
    /// Returns false when there are none (or a sheet is up): the caller decides.
    public func beginOpen() -> Bool {
        guard canOpenRecent, !recentEntries.isEmpty else { return false }
        sheet = .openSession
        return true
    }
}

/// The Open sheet: recents (↑/↓ move, ⏎ opens), and Choose Folder… for anything else.
@MainActor @Observable
public final class OpenSessionModel {
    public let entries: [RecentEntry]
    public var selection: UUID?

    public init(app: AppModel) {
        entries = app.recentEntries
        // The first one that isn't already open, else the first.
        selection = (entries.first { !$0.isCurrent } ?? entries.first)?.id
    }

    public var selected: RecentEntry? { entries.first { $0.id == selection } }
    public var canOpen: Bool { selected != nil }

    /// ↑/↓: to the next entry that way (stays put at the ends).
    public func move(_ delta: Int) {
        guard !entries.isEmpty else { return }
        guard let i = entries.firstIndex(where: { $0.id == selection }) else {
            selection = (delta < 0 ? entries.last : entries.first)?.id
            return
        }
        selection = entries[min(max(i + delta.signum(), 0), entries.count - 1)].id
    }
}
