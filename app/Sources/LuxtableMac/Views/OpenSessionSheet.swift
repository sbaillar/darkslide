import AppKit
import LuxtableCore
import LuxtableUI
import SwiftUI

/// File ▸ Open Folder… with recents: the recent sessions (name, folder, when it was last
/// opened, why one can't be), ↑/↓ to move, ⏎ to open; Choose Folder… for any other folder.
struct OpenSessionSheet: View {
    @Bindable var model: AppModel
    @State private var form: OpenSessionModel
    @FocusState private var listFocused: Bool

    init(model: AppModel) {
        self.model = model
        _form = State(initialValue: OpenSessionModel(app: model))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text("Open Session").font(.headline).foregroundStyle(DS.text)
            Text("RECENTS").font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
            ScrollView {
                VStack(spacing: 2) {
                    ForEach(form.entries) { e in
                        OpenSessionRow(entry: e, selected: form.selection == e.id)
                            .onTapGesture(count: 2) { open(e) }
                            .onTapGesture { form.selection = e.id }
                    }
                }
            }
            .frame(height: min(CGFloat(form.entries.count) * 44, 360))
            .focusable()
            .focused($listFocused)
            .focusEffectDisabled()
            .onKeyPress(.upArrow) { form.move(-1); return .handled }
            .onKeyPress(.downArrow) { form.move(1); return .handled }
            .accessibilityLabel("Recent sessions")
            HStack {
                Button("Choose Folder…") {
                    model.sheet = nil
                    OpenFolderPanel.run(model: model)
                }
                Button(CommandCatalogTitles.showLibrary) { // [library] the catalog: every session at once
                    model.sheet = nil
                    model.showLibrary()
                }
                .help(CommandCatalog.help(.showLibrary, model: model) ?? "")
                Spacer()
                Button("Cancel", role: .cancel) { model.sheet = nil }
                    .keyboardShortcut(.cancelAction)
                Button("Open") { if let e = form.selected { open(e) } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!form.canOpen)
            }
        }
        .padding(DS.Space.l)
        .frame(width: 520)
        .background(DS.panelBackground)
        .onAppear { listFocused = true }
    }

    private func open(_ e: RecentEntry) {
        model.sheet = nil
        LocatePanel.openRecent(e.session, model: model)
    }
}

/// One recent: name and folder, last opened or why it can't be opened.
struct OpenSessionRow: View {
    let entry: RecentEntry
    let selected: Bool

    var body: some View {
        HStack(spacing: DS.Space.s) {
            Image(systemName: entry.session.unavailable == nil ? "folder" : "questionmark.folder")
                .frame(width: 18)
                .foregroundStyle(selected ? DS.text : DS.textSecondary)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: DS.Space.xs) {
                    Text(entry.name).foregroundStyle(DS.text).lineLimit(1)
                    if entry.isCurrent { Text("Open").font(DS.Font.caption).foregroundStyle(DS.textSecondary) }
                }
                Text(entry.path).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                    .lineLimit(1).truncationMode(.middle)
            }
            Spacer()
            Text(entry.reason ?? entry.openedText() ?? "")
                .font(DS.Font.caption)
                .foregroundStyle(entry.reason == nil ? DS.textSecondary : DS.warningText)
        }
        .padding(.horizontal, DS.Space.s).padding(.vertical, 5)
        .background(selected ? DS.accent.opacity(0.25) : .clear, in: .rect(cornerRadius: DS.Radius.m))
        .opacity(entry.session.unavailable == nil ? 1 : 0.6)
        .contentShape(.rect)
        .help(LocatePanel.help(entry.session))
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(entry.name), \(entry.reason ?? entry.path)")
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
