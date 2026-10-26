import LuxtableCore
import SwiftUI

/// The AI Changes list (floating over the top-right of the window, like AI Optimize; Esc
/// or ⌥⇧⌘A closes it): every setting AI changed on the focused photo, with its old and new
/// values, who changed it and when. Click a line to show the tool; Undo puts one setting
/// back, Undo All every one (each is one ⌘Z step). Texts: `AIChangeText`, `AIChangesControl`.
package struct AIChangesPanel: View {
    package static let width: CGFloat = 420
    @Bindable var model: AppModel
    var maxHeight: CGFloat = .infinity
    @AccessibilityFocusState private var voiceOverHere: Bool

    package init(model: AppModel, maxHeight: CGFloat = .infinity) {
        self.model = model
        self.maxHeight = maxHeight
    }

    package var body: some View {
        let rows = model.focusedAIChangeRows
        VStack(alignment: .leading, spacing: DS.Space.s) {
            header
            Divider()
            if rows.isEmpty {
                Text(AIChangeText.empty)
                    .font(DS.Font.label)
                    .foregroundStyle(DS.textSecondary)
                    .padding(.vertical, DS.Space.s)
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: DS.Space.xs) {
                        ForEach(rows) { line($0) }
                    }
                    .padding(.vertical, DS.Space.xs)
                }
                .frame(maxHeight: min(420, max(120, maxHeight - 160)))
            }
            Divider()
            HStack {
                Spacer()
                let undoAll = AIChangesControl.undoAll(model)
                Button(undoAll.label) { Task { await model.undoAllAIChanges() } }
                    .disabled(!model.canUndoAllAIChanges)
                    .help(undoAll.hint ?? "")
            }
        }
        .padding(DS.Space.m)
        .frame(width: Self.width)
        .background(DS.panelBackground, in: .rect(cornerRadius: DS.Radius.l))
        .overlay(RoundedRectangle(cornerRadius: DS.Radius.l).strokeBorder(DS.hairline))
        .shadow(color: .black.opacity(0.35), radius: 16, y: 6)
        .frame(maxHeight: maxHeight, alignment: .top)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(AIChangeText.listTitle)
        .onExitCommand { model.aiChangesShown = false }
        .onAppear { voiceOverHere = true }
    }

    private var header: some View {
        HStack(spacing: DS.Space.s) {
            AIBadge(size: 13)
            Text(AIChangeText.listTitle)
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(DS.text)
                .accessibilityAddTraits(.isHeader)
                .accessibilityFocused($voiceOverHere)
            Spacer()
            if let name = model.focusedPhoto?.filename {
                Text(name).font(DS.Font.label).foregroundStyle(DS.textSecondary).lineLimit(1)
            }
            Button { model.aiChangesShown = false } label: {
                Image(systemName: "xmark").font(.system(size: 10, weight: .bold))
            }
            .buttonStyle(.plain)
            .foregroundStyle(DS.textSecondary)
            .help("Close (Esc)")
            .accessibilityLabel(AIChangesControl.close.label)
        }
    }

    private func line(_ r: AIChangeRow) -> some View {
        let undo = AIChangesControl.undo(r)
        return HStack(alignment: .center, spacing: DS.Space.s) {
            Button { ToolWorkspaceCommands.reveal(r.field.panel, model: model, layout: .shared) } label: {
                VStack(alignment: .leading, spacing: 1) {
                    HStack(spacing: DS.Space.xs) {
                        Text(r.setting).font(DS.Font.label.weight(.semibold)).foregroundStyle(DS.text)
                        Text(r.tool).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                    }
                    Text(r.detail).font(DS.Font.value).foregroundStyle(DS.text)
                    Text("\(r.source) · \(r.when)").font(DS.Font.caption).foregroundStyle(DS.textSecondary).lineLimit(1)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(.rect)
            }
            .buttonStyle(.plain)
            .help(r.tooltip)
            .accessibilityLabel(AIChangesControl.row(r).label)
            .accessibilityHint(AIChangesControl.row(r).hint ?? "")
            Button("Undo") {
                model.undoAIChanges([r.field])
            }
            .controlSize(.small)
            .disabled(!r.canUndo)
            .help(undo.hint ?? "")
            .accessibilityLabel(undo.label)
        }
        .padding(.horizontal, DS.Space.s)
        .padding(.vertical, DS.Space.xs)
        .overlay(alignment: .leading) {
            Capsule().fill(DS.aiChange).frame(width: 3).padding(.vertical, DS.Space.xxs).accessibilityHidden(true)
        }
    }
}
