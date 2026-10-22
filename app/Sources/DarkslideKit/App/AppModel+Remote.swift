import Foundation

/// Stores that can hand the link server an open session (its store root and
/// handle), so device writes go through the app's own session.
public protocol LinkSessionProviding: SessionStore {
    func linkSession(for session: SessionInfo) async -> LinkSession?
    /// The app's model now holds the session's edits, devices' included: the
    /// store stops keeping a device's edit when the app saves over it.
    func markDeviceEditsSeen(in session: SessionInfo) async
}

extension AppModel {
    /// Serve the open session on the link (none when closed).
    func serveCurrentSessionOnLink() async {
        guard let s = currentSession, let store = sessions as? any LinkSessionProviding else {
            remote.serve(nil)
            return
        }
        let link = await store.linkSession(for: s)
        // A newer session change started its own serve: this one is stale.
        guard !Task.isCancelled, currentSession?.id == s.id else { return }
        remote.serve(link)
    }

    /// After an open or reload put the stored edits in the model.
    /// Saves queued during the reload (a slider move, an Undo) go out first,
    /// while a device edit they would replace still counts as unseen, so the
    /// store keeps it as a variant; only then are the devices' edits adopted.
    func markDeviceEditsAdopted(_ session: SessionInfo) async {
        guard let link = sessions as? any LinkSessionProviding else { return }
        await saver.flush()
        await link.markDeviceEditsSeen(in: session)
    }

    /// A device pushed photos or edits: list the open session again if
    /// it was that one (the undo history carries over).
    func linkChanged(_ roots: [String]) async {
        guard let s = currentSession, let store = sessions as? any LinkSessionProviding,
              let root = await store.linkSession(for: s)?.root,
              roots.contains(where: { URL(fileURLWithPath: $0).standardizedFileURL == root.standardizedFileURL })
        else { return }
        await refreshCurrentSession(keepingHistory: true)
        show(Toast("Updated from your iPad", symbol: "ipad.landscape"))
    }
}
