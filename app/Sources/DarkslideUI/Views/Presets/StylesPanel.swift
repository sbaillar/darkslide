import DarkslideCore
import SwiftUI

/// Styles & presets: a library of looks, each shown on the focused photo. A click on a
/// style applies it on top of the current edit (one undo step; styles stack and never
/// reset the photo's own exposure); a click on a look replaces the photo's look, ⇧-click
/// or Add on Top stacks it. Hover previews a click in the viewer, and the current
/// adjustments can be saved, updated, renamed or duplicated as presets.
struct StylesPanel: View {
    @Bindable var model: AppModel
    var store = PresetStore.shared
    @State private var saving = false
    @State private var renaming: Preset?
    @State private var hovered: UUID?
    @State private var thumbnails: StyleThumbnails?
    @State private var suggestions: [LookSuggestion] = []

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            if let look = model.focusedEdit.look {
                LookIntensityRow(model: model, look: look)
            }
            originalRow
            if !suggestions.isEmpty {
                lookSection("AI Looks", suggestions.map { ($0.look, $0.reason) })
            }
            lookSection(LookPack.category, LookPack.all.map { ($0, $0.summary) })
            if !store.userPresets.isEmpty {
                section("My Presets", store.userPresets)
            }
            ForEach(Preset.builtInCategories, id: \.self) { c in
                section(c, store.styles(in: c))
            }
            HStack {
                Button {
                    saving = true
                } label: {
                    Label("Save Preset…", systemImage: "plus")
                }
                .disabled(model.focusedID == nil)
                .help("Save the current image's adjustments (groups you choose) as a preset")
                Button {
                    let edit = model.focusedEdit
                    PlatformPanels.copy(LookJSON.item(from: edit, name: "My Look"))
                    let left = LookJSON.leftOut(edit)
                    model.show(Toast(left.isEmpty ? "Copied as look JSON"
                        : "Copied as look JSON (without \(left.joined(separator: ", ")))", symbol: "doc.on.clipboard"))
                } label: {
                    Label("Copy as Look JSON", systemImage: "doc.on.clipboard")
                }
                .disabled(model.focusedID == nil)
                .help("Copy this photo's adjustments as a look for a plug-in's looks.json (docs/PLUGINS.md)")
                Spacer()
            }
            .controlSize(.small)
            if let err = store.lastError {
                Label(err, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.reject)
            }
            PanelNote(text: ViewerNotes.stylesHover(capabilities: model.viewer.capabilities))
        }
        .task(id: model.focusedID) {
            refreshThumbnails()
            suggestions = []
            // Arrowing through photos analyses only the one you stop on.
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else { return }
            let found = await model.lookSuggestions()
            if !Task.isCancelled { suggestions = found }
        }
        .onChange(of: model.focusedEdit) { refreshThumbnails() }
        .onChange(of: store.userPresets.map(\.id)) { refreshThumbnails() }
        // N4: a hover preview never outlives the panel (Tab/F/tab switch hide it mid-hover).
        .onDisappear {
            hovered = nil
            if model.optimizePanel == nil { model.viewer.setPreviewEdit(nil) } // AI Optimize owns the preview
            thumbnails?.cancel()
        }
        .sheet(isPresented: $saving) {
            SavePresetSheet(model: model, store: store) { saving = false }
        }
        .sheet(item: $renaming) { p in
            RenamePresetSheet(preset: p, store: store) { renaming = nil }
        }
    }

    private func refreshThumbnails() {
        if thumbnails == nil {
            thumbnails = StyleThumbnails(render: StyleThumbnails.render(with: model.tools))
        }
        thumbnails?.refresh(photo: model.focusedPhoto, edit: model.focusedEdit,
                            presets: LookPack.all.map(\.preset) + store.all)
    }

    /// The photo as shot: hover previews it, a click goes back to it (one undo step), so
    /// trying styles never means undoing all the way back.
    private var originalRow: some View {
        let id = StyleThumbnails.originalID
        let isOriginal = model.focusedEdit == EditState()
        return Button {
            previewHover(nil)
            model.resetToOriginal()
        } label: {
            PresetRow(title: "Original", symbol: "photo", detail: isOriginal ? "Current" : "No adjustments",
                      summary: "Back to the photo as shot, without any adjustments (⌘Z undoes it)", hovered: hovered == id, tile: thumbnails?.tile(id), applied: isOriginal)
        }
        .buttonStyle(.plain)
        .disabled(model.focusedID == nil)
        .onHover { inside in
            hovered = inside ? id : (hovered == id ? nil : hovered)
            guard model.optimizePanel == nil else { return } // AI Optimize owns the preview
            model.viewer.setPreviewEdit(inside && model.focusedID != nil ? EditState() : nil)
        }
        .accessibilityLabel("Back to the original")
    }

    private func section(_ title: String, _ presets: [Preset]) -> some View {
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            Text(title.uppercased()).font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
            ForEach(presets) { p in
                Button {
                    previewHover(nil)
                    model.applyPresets([p])
                } label: {
                    PresetRow(preset: p, hovered: hovered == p.id, tile: thumbnails?.tile(p.id))
                }
                .buttonStyle(.plain)
                .onHover { inside in
                    hovered = inside ? p.id : (hovered == p.id ? nil : hovered)
                    previewHover(inside ? p : nil)
                }
                .contextMenu { menu(for: p) }
                .accessibilityLabel("Apply \(p.isBuiltIn ? "style" : "preset") \(p.name)")
            }
        }
    }

    /// Looks: a click applies (one undo step), hover previews; `reason` is the row's help.
    private func lookSection(_ title: String, _ looks: [(Look, String)]) -> some View {
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            HStack(spacing: DS.Space.xxs) {
                if title != LookPack.category { Image(systemName: "sparkles").font(DS.Font.caption).foregroundStyle(DS.accent) }
                Text(title.uppercased()).font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
            }
            ForEach(looks, id: \.0.id) { look, reason in
                Button {
                    previewHover(nil)
                    model.applyLook(look, stacking: PlatformInput.shiftDown)
                } label: {
                    VStack(alignment: .leading, spacing: 0) {
                        PresetRow(preset: look.preset, hovered: hovered == look.preset.id,
                                  tile: thumbnails?.tile(look.preset.id), applied: model.focusedEdit.look?.id == look.id)
                        if title != LookPack.category {
                            Text(reason).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                                .padding(.horizontal, DS.Space.s).fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }
                .buttonStyle(.plain)
                .help("\(reason) (⇧-click to add on top of the current look)")
                .onHover { inside in
                    hovered = inside ? look.preset.id : (hovered == look.preset.id ? nil : hovered)
                    previewHover(inside ? look.preset : nil)
                }
                .contextMenu {
                    Button("Add on Top") {
                        previewHover(nil)
                        model.applyLook(look, stacking: true)
                    }
                    .disabled(!model.editAllSelected && (model.focusedEdit.look == nil || model.focusedEdit.look?.id == look.id))
                    Button("Duplicate to My Presets") { store.duplicate(look.preset) }
                }
                .accessibilityLabel("Apply look \(look.name)")
                .accessibilityAction(named: "Add on Top") { model.applyLook(look, stacking: true) }
            }
        }
    }

    @ViewBuilder private func menu(for p: Preset) -> some View {
        Button("Apply") { model.applyPresets([p]) }
        Button("Duplicate to My Presets") { store.duplicate(p) }
        if !p.isBuiltIn {
            Button("Update from Current Photo") {
                store.update(p.id, from: model.focusedEdit)
                thumbnails?.invalidate(p.id)
            }
            .disabled(model.focusedID == nil)
            Button("Rename…") { renaming = p }
            Toggle("Apply Automatically on Import", isOn: Binding(
                get: { p.autoApplyOnImport },
                set: { store.setAutoApply($0 ? p.id : nil) }))
            Divider()
            Button("Delete", role: .destructive) { store.delete(p.id) }
        }
    }

    /// Hover preview: the viewer renders the preset applied to the current edit (nil restores).
    private func previewHover(_ preset: Preset?) {
        guard model.optimizePanel == nil else { return } // AI Optimize owns the preview
        model.viewer.setPreviewEdit(preset.flatMap { model.previewEdit(applying: $0) })
    }
}

/// The applied look's Intensity slider: 0 restores the photo as it was before the look.
private struct LookIntensityRow: View {
    @Bindable var model: AppModel
    let look: AppliedLookEdit

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            Text("LOOK: \((LookPack.look(id: look.id)?.name ?? "Custom").uppercased())")
                .font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
            ToolSlider(spec: LookSpecs.intensity, value: look.intensity, onEnd: { model.endEditGesture() }) { v in
                model.setLookIntensity(v)
            }
        }
    }
}

enum LookSpecs {
    static let intensity = SliderSpec("Intensity", 0...1, default: 1, step: 0.01, format: .percent,
                                      help: "How strongly the look is applied; 0 shows the photo as it was before it.")
}

private struct PresetRow: View {
    let title: String
    let symbol: String
    let detail: String
    var autoApply = false
    var summary = ""
    let hovered: Bool
    /// The tile, and whether it's the previous edit's (dimmed until the new one renders).
    let tile: (image: CGImage, isStale: Bool)?
    var applied = false

    init(preset: Preset, hovered: Bool, tile: (image: CGImage, isStale: Bool)?, applied: Bool = false) {
        self.init(title: preset.name, symbol: preset.isBuiltIn ? "camera.filters" : "slider.horizontal.3",
                  detail: preset.groups.count == 1 ? preset.groups.first!.title : "\(preset.groups.count) groups",
                  hovered: hovered, tile: tile, applied: applied)
        autoApply = preset.autoApplyOnImport
        summary = preset.summary
    }

    init(title: String, symbol: String, detail: String, summary: String = "", hovered: Bool,
         tile: (image: CGImage, isStale: Bool)?, applied: Bool = false) {
        self.title = title
        self.summary = summary
        self.symbol = symbol
        self.detail = detail
        self.hovered = hovered
        self.tile = tile
        self.applied = applied
    }

    var body: some View {
        HStack(spacing: DS.Space.s) {
            Group {
                if let tile {
                    Image(decorative: tile.image, scale: 1).resizable().aspectRatio(contentMode: .fill)
                        .opacity(tile.isStale ? 0.55 : 1)
                        .animation(.easeOut(duration: 0.15), value: tile.isStale)
                } else {
                    Image(systemName: symbol)
                        .foregroundStyle(hovered ? DS.accent : DS.textSecondary)
                }
            }
            .frame(width: 44, height: 30)
            .clipShape(.rect(cornerRadius: 3))
            Text(title).font(DS.Font.label).foregroundStyle(applied ? DS.accent : DS.text)
            if autoApply {
                Image(systemName: "square.and.arrow.down").font(.system(size: 9)).foregroundStyle(DS.accent)
                    .help("Applied automatically on import")
            }
            Spacer()
            Text(detail)
                .font(DS.Font.caption).foregroundStyle(DS.textTertiary)
        }
        .padding(.horizontal, DS.Space.s)
        .padding(.vertical, DS.Space.xxs)
        .background(hovered ? DS.accent.opacity(0.12) : .clear, in: .rect(cornerRadius: DS.Radius.m))
        .contentShape(.rect)
        .help(summary)
    }
}

/// Name + group checkboxes for a new preset.
private struct SavePresetSheet: View {
    @Bindable var model: AppModel
    let store: PresetStore
    let done: () -> Void
    @State private var name = ""
    @State private var groups: Set<AdjustGroup> = []
    @State private var addTone = true

    var body: some View {
        let edit = model.focusedEdit
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text("Save Preset").font(.headline)
            TextField("Name", text: $name).textFieldStyle(.roundedBorder)
            Text("Include").font(DS.Font.label).foregroundStyle(DS.textSecondary)
            LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], alignment: .leading, spacing: DS.Space.xs) {
                ForEach(AdjustGroup.allCases.filter { $0 != .layers }) { g in
                    Toggle(g.title, isOn: Binding(get: { groups.contains(g) },
                                                  set: { if $0 { groups.insert(g) } else { groups.remove(g) } }))
                        .toggleStyle(.checkbox)
                        .font(DS.Font.label)
                }
            }
            Toggle("Add exposure and HDR to each photo's own values", isOn: $addTone)
                .toggleStyle(.checkbox).font(DS.Font.label)
                .help("On: the preset adjusts a photo's exposure instead of replacing it")
            HStack {
                Spacer()
                Button("Cancel", role: .cancel, action: done).keyboardShortcut(.cancelAction)
                Button("Save") {
                    let rel: Set<AdjustGroup> = addTone ? Preset.defaultRelative(for: groups) : []
                    if store.save(name: name, edit: edit, groups: groups, relative: rel) != nil { done() }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty || groups.isEmpty)
            }
        }
        .padding(DS.Space.l)
        .frame(width: 420)
        .onAppear { groups = AdjustGroup.changed(in: edit).subtracting([.layers]) }
    }
}

private struct RenamePresetSheet: View {
    let preset: Preset
    let store: PresetStore
    let done: () -> Void
    @State private var name = ""

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text("Rename Preset").font(.headline)
            TextField("Name", text: $name).textFieldStyle(.roundedBorder)
            HStack {
                Spacer()
                Button("Cancel", role: .cancel, action: done).keyboardShortcut(.cancelAction)
                Button("Rename") {
                    store.rename(preset.id, to: name)
                    done()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(DS.Space.l)
        .frame(width: 360)
        .onAppear { name = preset.name }
    }
}
