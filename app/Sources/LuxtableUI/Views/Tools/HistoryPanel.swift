import LuxtableCore
import SwiftUI

/// [history] The focused photo's edit steps, newest on top (Lightroom's History). A click
/// jumps the photo to that step (one undo step); rows above it stay, dimmed, until the
/// next edit branches off. Hover previews a step in the viewer, like hovering a style.
struct HistoryPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        let history = model.focusedHistory
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            if history.isEmpty {
                PanelEmptyState(symbol: "clock.arrow.circlepath", text: "No edits yet.")
            }
            ForEach(history.rows) { node in
                row(node, current: node === history.current, undone: history.isAfterCurrent(node))
            }
            if history.isTrimmed {
                Text("Older steps not shown").font(DS.Font.caption).foregroundStyle(DS.textTertiary)
                    .padding(.horizontal, DS.Space.s)
            }
            if history.rows.count > 1 {
                HStack {
                    Button("Clear History") {
                        model.previewHistory(nil)
                        model.clearHistory()
                    }
                    .help("Remove this photo's steps; its edit stays. This can't be undone.")
                    Spacer()
                }
                .controlSize(.small)
                .padding(.top, DS.Space.xxs)
            }
        }
        // A hover preview never outlives the panel or the photo it was made for.
        .onDisappear { model.previewHistory(nil) }
        .onChange(of: model.focusedID) { model.previewHistory(nil) }
    }

    private func row(_ node: HistoryNode, current: Bool, undone: Bool) -> some View {
        let hovered = model.historyPreview?.id == node.id
        return Button {
            model.previewHistory(nil)
            model.jumpToHistory(node)
        } label: {
            HStack(spacing: DS.Space.s) {
                Text(node.label).font(DS.Font.label)
                    .foregroundStyle(undone ? DS.textTertiary : DS.text)
                Spacer(minLength: 0)
                if current { Image(systemName: "checkmark").font(DS.Font.caption).foregroundStyle(DS.accent) }
            }
            .padding(.horizontal, DS.Space.s)
            .padding(.vertical, DS.Space.xxs)
            .contentShape(.rect)
            .background(hovered || current ? DS.accent.opacity(current ? 0.18 : 0.12) : .clear,
                        in: .rect(cornerRadius: DS.Radius.m))
        }
        .buttonStyle(.plain)
        .help(node.time.formatted(date: .omitted, time: .standard))
        .onHover { inside in
            if inside {
                model.previewHistory(node)
            } else if hovered {
                model.previewHistory(nil)
            }
        }
        .accessibilityLabel("Go to \(node.label)")
        .accessibilityAddTraits(current ? .isSelected : [])
        .accessibilityValue(undone ? "Undone" : "")
    }
}
