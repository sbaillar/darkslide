import SwiftUI

/// The Metadata panel's IPTC fields: edits the selection; a field whose photos
/// differ shows "Mixed" and is written only when the user types into it.
struct IPTCFieldsView: View {
    @Bindable var model: AppModel
    @State private var presetName = ""
    /// The credits "Save as Preset…" was chosen for, while the name is typed.
    @State private var pending: IPTCMetadata?
    private var store: MetadataPresetStore { MetadataPresetStore.shared }

    var body: some View {
        let targets = model.metadataTargets
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            HStack {
                Text(targets.count > 1 ? "IPTC · \(targets.count) photos" : "IPTC").font(DS.Font.label)
                Spacer()
                presetMenu(targets)
            }
            ForEach(IPTCMetadata.Field.allCases) { f in
                IPTCFieldRow(model: model, field: f, shown: MetadataEditing.shown(f, in: targets))
                    .disabled(targets.isEmpty)
            }
            if let fields = pending {
                HStack(spacing: DS.Space.xs) {
                    TextField("Preset name", text: $presetName)
                        .textFieldStyle(.roundedBorder)
                        .controlSize(.small)
                        .onSubmit {
                            if store.save(name: presetName, fields: fields) != nil {
                                presetName = ""
                                pending = nil
                            }
                        }
                    Button("Cancel") {
                        presetName = ""
                        pending = nil
                    }
                    .controlSize(.small)
                }
            }
            if let e = store.lastError { PanelNote(text: e) }
        }
    }

    private func presetMenu(_ targets: [Photo]) -> some View {
        Menu("Presets") {
            ForEach(store.presets) { p in
                Button("Apply “\(p.name)”") { model.applyMetadataPreset(p) }.disabled(targets.isEmpty)
            }
            if !store.presets.isEmpty { Divider() }
            Button("Save Credits as Preset…") { pending = targets.first?.metadata.credits }
                .disabled(targets.count != 1 || targets[0].metadata.credits.isEmpty)
            Menu("Apply on Import") {
                Button(store.importPreset == nil ? "✓ None" : "None") { store.setImportPreset(nil) }
                ForEach(store.presets) { p in
                    Button(p.applyOnImport ? "✓ \(p.name)" : p.name) { store.setImportPreset(p.id) }
                }
            }
            Menu("Delete") {
                ForEach(store.presets) { p in Button(p.name) { store.delete(p.id) } }
            }
            .disabled(store.presets.isEmpty)
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .controlSize(.small)
        .help("Apply, save or choose a metadata preset (e.g. creator and copyright) for imports")
    }
}

/// One field: shows the selection's value (or "Mixed"). Editing starts when it gains
/// focus, for the photos selected then; Return or leaving the field commits to them.
private struct IPTCFieldRow: View {
    let model: AppModel
    let field: IPTCMetadata.Field
    let shown: MetadataEditing.Shown
    @State private var text = ""
    @State private var edit: MetadataEdit?
    @FocusState private var focused: Bool

    var body: some View {
        HStack(alignment: field.isLong ? .top : .center, spacing: DS.Space.xs) {
            Text(field.label).font(DS.Font.label).foregroundStyle(DS.textSecondary).frame(width: 72, alignment: .leading)
            TextField(shown == .mixed ? "Mixed" : "", text: $text, axis: field.isLong ? .vertical : .horizontal)
                .lineLimit(field.isLong ? 1...4 : 1...1)
                .textFieldStyle(.roundedBorder)
                .controlSize(.small)
                .focused($focused)
                .onSubmit { edit = edit.map { model.commitMetadata($0, typed: text) } }
                .onChange(of: focused) { _, now in
                    if now {
                        edit = model.beginMetadataEdit(field)
                    } else if let e = edit {
                        model.commitMetadata(e, typed: text)
                        edit = nil
                    }
                }
                .accessibilityLabel(field.label)
            if shown == .mixed {
                Button { model.clearMetadata(field) } label: { Image(systemName: "xmark.circle") }
                    .buttonStyle(.plain)
                    .foregroundStyle(DS.textSecondary)
                    .help("Clear for All: empty \(field.label) on every selected photo")
                    .accessibilityLabel("Clear \(field.label) for all")
            }
        }
        .onAppear { text = MetadataEditing.initialText(shown) }
        .onChange(of: shown) { _, new in if !focused { text = MetadataEditing.initialText(new) } }
        .onChange(of: model.selectedIDs) { _, _ in
            // The selection changed under an open edit: finish it for the photos it
            // was for, then edit the new selection.
            guard focused, let e = edit, e.ids != model.metadataTargets.map(\.id) else { return }
            model.commitMetadata(e, typed: text)
            text = MetadataEditing.initialText(shown)
            edit = model.beginMetadataEdit(field)
        }
    }
}
