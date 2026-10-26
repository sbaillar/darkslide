import LuxtableCore
import LuxtableUI
import SwiftUI

/// Launch prompt for an import that didn't finish (lt-import journal).
struct ImportResumePrompt: ViewModifier {
    @Bindable var model: AppModel

    func body(content: Content) -> some View {
        content.alert("Unfinished import", isPresented: presented, presenting: model.pendingImport) { p in
            Button("Resume…") { Task { await model.resumePendingImport(p) } }
            Button("Discard Unfinished Copies", role: .destructive) { Task { await model.discardPendingImport(p) } }
            Button("Later", role: .cancel) { model.nextPendingImport() }
        } message: { p in
            Text(ImportWording.resumePrompt(sessionName: p.sessionName, done: p.done, pending: p.pending + p.failed))
        }
    }

    private var presented: Binding<Bool> {
        Binding(get: { model.pendingImport != nil && model.sheet == nil },
                set: { _ in }) // each button decides what comes next (the next unfinished import)
    }
}
