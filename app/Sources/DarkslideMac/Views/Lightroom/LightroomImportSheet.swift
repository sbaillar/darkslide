import AppKit
import DarkslideCore
import DarkslideUI
import SwiftUI

/// Import from Lightroom: choose a catalog, a folder of XMP sidecars or presets; tick what
/// to bring over; read the dry run; import. Photos of the catalog that aren't in this
/// session are listed by folder, each with a button that opens it as a session.
struct LightroomImportSheet: View {
    let app: AppModel
    @Bindable var model: LightroomImportModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text(model.title).font(.headline).foregroundStyle(DS.text)
                .accessibilityAddTraits(.isHeader)
            sourceLine
            content
            buttons
        }
        .padding(DS.Space.l)
        .frame(width: 580)
        .background(DS.panelBackground)
    }

    private var sourceLine: some View {
        HStack(spacing: DS.Space.s) {
            Text(model.source?.path ?? "Choose a Lightroom catalog (.lrcat), a folder of XMP sidecars, or presets")
                .font(DS.Font.label).foregroundStyle(model.source == nil ? DS.textSecondary : DS.text)
                .lineLimit(1).truncationMode(.middle)
            Spacer()
            Button("Choose\u{2026}") { choose() }.disabled(isBusy)
        }
    }

    private var isBusy: Bool {
        switch model.phase {
        case .loading, .previewing, .importing, .reverting: true
        default: false
        }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = true
        panel.allowsMultipleSelection = false
        panel.message = "Choose a Lightroom catalog (.lrcat), a folder of XMP sidecars, or preset files"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        Task { await model.choose(url) }
    }

    @ViewBuilder private var content: some View {
        switch model.phase {
        case .choosing:
            Text(model.session == nil
                 ? "Presets import into My Presets. To bring in a catalog or sidecars, open the folder with your RAW files first."
                 : "Lightroom's files are only read, never changed. Edits go into \u{201c}\(model.session?.name ?? "")\u{201d}.")
                .font(DS.Font.label).foregroundStyle(DS.textSecondary)
        case .loading:
            busy("Reading\u{2026} A large catalog is copied first (the original isn't touched).")
        case .previewing:
            busy("Checking what would change (nothing is written)\u{2026}")
        case .preview(let s):
            categories
            summary(s)
            if !s.conflicts.isEmpty {
                Toggle("Overwrite my Darkslide edits on conflicts", isOn: $model.overwriteEdits)
                    .font(DS.Font.label).foregroundStyle(DS.text)
                    .accessibilityHint("Off: photos you changed in Darkslide since the last import keep your values")
            }
        case .importing:
            busy("Importing\u{2026} The Lightroom files are only read.")
        case .done(let s):
            summary(s)
            if model.canRevert {
                Text(LightroomImportModel.revertHint).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
        case .reverting:
            busy("Reverting the import\u{2026}")
        case .reverted(let r):
            Text(r.message).font(DS.Font.label).foregroundStyle(DS.text)
        case .presets(let items):
            presets(items)
        case .presetsDone(let message):
            Text(message).font(DS.Font.label).foregroundStyle(DS.text)
        case .failed(let message):
            HStack(alignment: .firstTextBaseline, spacing: DS.Space.s) {
                Image(systemName: "xmark.octagon").foregroundStyle(DS.reject).accessibilityHidden(true)
                Text(message).font(DS.Font.label).foregroundStyle(DS.text)
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Failed: \(message)")
        }
    }

    private func busy(_ text: String) -> some View {
        HStack(spacing: DS.Space.s) {
            ProgressView().controlSize(.small)
            Text(text).font(DS.Font.label).foregroundStyle(DS.text)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(text)
    }

    private var categories: some View {
        HStack(alignment: .top, spacing: DS.Space.l) {
            group("Library", LRCategory.organise)
            group("Develop", LRCategory.develop)
        }
    }

    private func group(_ title: String, _ items: [LRCategory]) -> some View {
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            Text(title.uppercased()).font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
                .accessibilityLabel(title).accessibilityAddTraits(.isHeader)
            ForEach(items, id: \.self) { c in
                Toggle(model.label(c), isOn: Binding(get: { model.categories.contains(c) }, set: { _ in model.toggle(c) }))
                    .font(DS.Font.label).foregroundStyle(DS.text)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func summary(_ s: LRImportSummary) -> some View {
        ImportReportView(summary: s, folders: model.folderRows) { await model.openAsSession($0) }
    }

    private func presets(_ items: [LRPresetItem]) -> some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            ScrollView {
                VStack(alignment: .leading, spacing: DS.Space.xs) {
                    ForEach(items) { item in
                        VStack(alignment: .leading, spacing: 0) {
                            Text((item.group.map { "\($0) \u{25B8} " } ?? "") + item.name)
                                .font(DS.Font.label).foregroundStyle(DS.text)
                            if let why = item.unavailable {
                                Text("Can't be imported: \(why)").font(DS.Font.caption).foregroundStyle(DS.warningText)
                            }
                            ForEach(item.notes, id: \.self) { Text($0).font(DS.Font.caption).foregroundStyle(DS.textSecondary) }
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 280)
            Toggle("Replace presets that are already there", isOn: $model.replacePresets)
                .font(DS.Font.label).foregroundStyle(DS.text)
        }
    }

    @ViewBuilder private var buttons: some View {
        HStack {
            Spacer()
            switch model.phase {
            case .choosing:
                Button("Cancel", role: .cancel) { close() }.keyboardShortcut(.cancelAction)
                Button("Choose\u{2026}") { choose() }.keyboardShortcut(.defaultAction)
            case .preview:
                Button("Cancel", role: .cancel) { close() }.keyboardShortcut(.cancelAction)
                Button(model.overwriteEdits ? "Import and Overwrite" : "Import") { Task { await model.run() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!model.canImport)
            case .presets:
                let n = model.importablePresets.count
                Button("Cancel", role: .cancel) { close() }.keyboardShortcut(.cancelAction)
                Button("Import \(n) Preset\(n == 1 ? "" : "s")") { model.importPresets() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(n == 0)
            case .loading, .previewing, .importing, .reverting:
                Button("Cancel", role: .cancel) { close() }.disabled(model.phase == .importing || model.phase == .reverting)
            case .done:
                if model.canRevert {
                    Button("Revert Import") { Task { await model.revert() } }
                        .accessibilityHint("Puts back what the Lightroom imports since the last revert changed")
                }
                Button("Done") { close() }.keyboardShortcut(.defaultAction)
            case .reverted, .failed, .presetsDone:
                if case .failed = model.phase {
                    if model.canRevert {
                        Button("Revert Import") { Task { await model.revert() } }
                            .accessibilityHint("Puts back what the failed import had already changed")
                    }
                    Button("Choose\u{2026}") { choose() }
                }
                Button("Done") { close() }.keyboardShortcut(.defaultAction)
            }
        }
    }

    private func close() {
        app.sheet = nil
        Task { await model.release() }
    }
}

/// File ▸ Revert Lightroom Import asks first.
struct LightroomRevertDialog: ViewModifier {
    @Bindable var model: AppModel

    func body(content: Content) -> some View {
        content.confirmationDialog("Revert Lightroom Import?", isPresented: $model.confirmingLightroomRevert,
                                   titleVisibility: .visible) {
            Button("Revert", role: .destructive) { Task { await model.revertLightroomImport() } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Takes back every Lightroom import since the last revert. Changes you made in Darkslide afterwards are kept.")
        }
    }
}
