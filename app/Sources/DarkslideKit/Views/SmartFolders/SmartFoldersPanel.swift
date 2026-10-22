import SwiftUI

/// Library ▸ Smart Folders: built-ins (exact ratings, unrated, picked, rejected,
/// tags in use) then the user's. Clicking one filters the browser to it.
struct SmartFoldersPanel: View {
    @Bindable var model: AppModel
    @State private var editing: SmartFolderEditorTarget?

    var body: some View {
        let sf = model.smartFolders
        let selected = model.filter.smartFolder?.id
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            row(symbol: "photo.on.rectangle", tint: DS.textSecondary, name: "All Photos",
                count: model.photoCount, selected: selected == nil, shortcut: "⌘0") { model.selectSmartFolder(nil) }
            ForEach(sf.visibleFolders) { f in
                row(symbol: f.symbol, tint: tint(f), name: f.name, count: sf.counts[f.id, default: 0],
                    selected: selected == f.id, shortcut: f.exactRating.map { "⌘\($0)" }) {
                    model.selectSmartFolder(selected == f.id ? nil : f.id)
                }
                .contextMenu { menu(f) }
            }
            Button {
                editing = .new
            } label: {
                Label("New Smart Folder…", systemImage: "plus").font(DS.Font.label)
            }
            .buttonStyle(.plain)
            .foregroundStyle(DS.textSecondary)
            .padding(.horizontal, DS.Space.s)
            .padding(.top, DS.Space.xs)
            .disabled(model.currentSession == nil)
            .help("Make a folder from any rating, tag, flag, camera, lens, date, ISO or keyword rule")
            if let e = sf.lastError { PanelNote(text: e) }
        }
        .sheet(item: $editing) { target in
            SmartFolderEditor(target: target) { draft in
                Task { await save(draft, target: target) }
            }
        }
    }

    private func tint(_ f: SmartFolder) -> Color {
        if f.isTagFolder, let t = f.criteria.tags.first { return DS.tagColor(t) }
        return f.exactRating != nil ? .yellow : DS.textSecondary
    }

    @ViewBuilder private func menu(_ f: SmartFolder) -> some View {
        Toggle("Mirror to Finder", isOn: Binding(get: { f.mirror }, set: { on in
            Task { await model.smartFolders.setMirror(f, on: on, photos: model.photos) }
        }))
        .help("Keep a Finder folder of links to these originals (never copies)")
        if !f.isBuiltin {
            Divider()
            Button("Edit Smart Folder…") { editing = .edit(f) }
            Button("Delete Smart Folder", role: .destructive) {
                if model.filter.smartFolder?.id == f.id { model.selectSmartFolder(nil) }
                Task { await model.smartFolders.delete(f, photos: model.photos) }
            }
        }
    }

    private func save(_ draft: SmartFolderDraft, target: SmartFolderEditorTarget) async {
        let sf = model.smartFolders
        switch target {
        case .new:
            if let f = await sf.create(name: draft.name, criteria: draft.criteria, photos: model.photos) {
                model.selectSmartFolder(f.id)
            }
        case .edit(let old):
            var f = old
            f.name = draft.name.trimmingCharacters(in: .whitespacesAndNewlines)
            f.criteria = draft.criteria
            await sf.update(f, photos: model.photos)
            if model.filter.smartFolder?.id == f.id { model.selectSmartFolder(f.id) }
        }
    }

    private func row(symbol: String, tint: Color, name: String, count: Int, selected: Bool, shortcut: String?,
                     action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: DS.Space.s) {
                Image(systemName: symbol).foregroundStyle(tint).frame(width: 16)
                Text(name).font(DS.Font.label).lineLimit(1)
                Spacer()
                Text("\(count)").font(DS.Font.value).foregroundStyle(DS.textSecondary).monospacedDigit()
            }
            .padding(.horizontal, DS.Space.s)
            .padding(.vertical, 3)
            .background(selected ? DS.accent.opacity(0.12) : .clear, in: .rect(cornerRadius: DS.Radius.m))
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .help(shortcut.map { "Show \(name.lowercased()) (\($0))" } ?? "Show \(name.lowercased())")
        .accessibilityLabel("\(name), \(count) photos")
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
