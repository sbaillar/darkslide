import DarkslideCore
import SwiftUI

/// The non-blocking banner over the viewer (and over the grid, for a whole session):
/// never a silent viewer. Pulses when an edit is made that can't be previewed.
package struct ProblemBanner: View {
    @Bindable var model: AppModel
    /// Only the session-level message (the grid has no single photo).
    var sessionOnly = false
    @State private var pulse = false

    package init(model: AppModel, sessionOnly: Bool = false) {
        self.model = model
        self.sessionOnly = sessionOnly
    }

    package var body: some View {
        if let p = model.viewer.problem, !sessionOnly || p.isSessionWide {
            HStack(spacing: DS.Space.s) {
                Image(systemName: p.symbol).foregroundStyle(DS.reject)
                Text(p.message)
                    .font(DS.Font.label)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: DS.Space.s)
                ForEach(p.actions, id: \.action) { item in
                    Button(item.title) { perform(item.action) }.controlSize(.small)
                }
            }
            .padding(.horizontal, DS.Space.m)
            .padding(.vertical, DS.Space.s)
            .background(DS.panelBackground.opacity(0.95), in: .rect(cornerRadius: DS.Radius.m))
            .overlay {
                RoundedRectangle(cornerRadius: DS.Radius.m)
                    .strokeBorder(DS.reject.opacity(pulse ? 1 : 0.5), lineWidth: pulse ? 2 : 1)
            }
            .scaleEffect(pulse ? 1.02 : 1)
            .padding(DS.Space.m)
            .frame(maxWidth: 760)
            .onChange(of: model.viewer.problemPulse) { _, _ in
                withAnimation(.easeOut(duration: 0.12)) { pulse = true }
                withAnimation(.easeIn(duration: 0.35).delay(0.12)) { pulse = false }
            }
            .accessibilityElement(children: .contain)
            .accessibilityLabel(p.message)
        }
    }

    private func perform(_ action: ViewerProblem.Action) {
        switch action {
        case .locateFolder:
            guard let s = model.currentSession, let url = LocatePanel.folder(for: s) else { return }
            Task { await model.locateSession(s, at: url) }
        case .removeFromRecents:
            Task { await model.forgetCurrentSession() }
        case .locateFile:
            guard let id = model.focusedID, let p = model.focusedPhoto, let url = LocatePanel.file(for: p) else { return }
            Task { await model.locate(id, at: url) }
        case .removePhoto:
            guard let id = model.focusedID, model.canRemoveMissing(id), ConfirmRemove.run(count: 1) else { return }
            Task { await model.removeMissing([id]) }
        case .retry:
            model.viewer.retryRender()
        }
    }
}

/// Asks before Remove forgets ratings and edits.
@MainActor
enum ConfirmRemove {
    static func run(count: Int) -> Bool {
        let c = MissingActions.confirmRemove(count: count)
        return PlatformPanels.confirmDestructive(title: c.title, message: c.message, button: c.button)
    }
}

/// "Where is it now?" panels for a missing folder or file.
@MainActor
package enum LocatePanel {
    static func folder(for s: SessionInfo) -> URL? {
        PlatformPanels.chooseItem(files: false, directories: true, prompt: "Locate",
                                  message: "Where is “\(s.name)” now? Choose the folder that holds its photos.",
                                  directory: existingAncestor(of: s.url))
    }

    static func file(for p: Photo) -> URL? {
        PlatformPanels.chooseItem(prompt: "Locate", message: "Where is \(p.filename) now?",
                                  directory: p.url.map { existingAncestor(of: $0.deletingLastPathComponent()) })
    }

    /// A recents row was clicked: open it, or, when its folder isn't there, ask where it is.
    package static func openRecent(_ s: SessionInfo, model: AppModel) {
        // Gone, or its drive isn't at that path (maybe mounted under another name): Locate.
        guard s.unavailable?.folderMissing == true, !FileManager.default.fileExists(atPath: s.url.path) else {
            Task { await model.open(s) }
            return
        }
        guard let url = folder(for: s) else { return }
        Task { await model.locateSession(s, at: url) }
    }

    /// A recents row's tooltip: where it is, and why it's greyed out.
    package static func help(_ s: SessionInfo) -> String {
        switch s.unavailable {
        case nil: s.url.path
        case .notFound: "\(s.url.path): not found. Click to locate it."
        case .noPhotos: "\(s.url.path): no photos found. Open it to locate them."
        case .driveNotConnected:
            "\(s.url.path): its drive isn’t connected. Connect it, or click to locate it (it may be mounted under another name)."
        }
    }

    /// The nearest folder above `url` that still exists (where the panel starts).
    private static func existingAncestor(of url: URL) -> URL {
        var u = url
        while u.path != "/", !FileManager.default.fileExists(atPath: u.path) { u.deleteLastPathComponent() }
        return u
    }
}
