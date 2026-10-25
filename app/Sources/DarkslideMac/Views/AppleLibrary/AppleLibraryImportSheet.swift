import AppKit
import DarkslideCore
import DarkslideUI
import SwiftUI

/// Import from Aperture or iPhoto: choose a library; tick what to bring over; read the
/// dry run; import. Photos of the library that aren't in this session are listed by
/// folder (the library's own originals folder first), each with a button that opens it
/// as a session.
struct AppleLibraryImportSheet: View {
    let app: AppModel
    @Bindable var model: AppleLibraryImportModel

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
            Text(model.info?.line ?? model.source?.path ?? "Choose an Aperture library (.aplibrary) or an iPhoto library")
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
        panel.treatsFilePackagesAsDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = "Choose an Aperture library (.aplibrary) or an iPhoto library"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        Task { await model.choose(url) }
    }

    @ViewBuilder private var content: some View {
        switch model.phase {
        case .choosing:
            Text(model.session == nil
                 ? "The library is only read, never changed. Its photos can be opened as a session once it is chosen."
                 : "The library is only read, never changed. Ratings, keywords and edits go into \u{201c}\(model.session?.name ?? "")\u{201d}.")
                .font(DS.Font.label).foregroundStyle(DS.textSecondary)
        case .loading:
            busy("Reading the library\u{2026} Its database is copied first (the library isn't touched).")
        case .previewing:
            busy("Checking what would change (nothing is written)\u{2026}")
        case .preview(let s):
            categories
            ImportReportView(summary: s, folders: model.folderRows) { await model.openAsSession($0) }
            if !s.conflicts.isEmpty {
                Toggle("Overwrite my Darkslide edits on conflicts", isOn: $model.overwriteEdits)
                    .font(DS.Font.label).foregroundStyle(DS.text)
                    .accessibilityHint("Off: photos you changed in Darkslide since the last import keep your values")
            }
        case .importing:
            busy("Importing\u{2026} The library is only read.")
        case .done(let s):
            ImportReportView(summary: s, folders: model.folderRows) { await model.openAsSession($0) }
            if model.canRevert {
                Text(AppleLibraryImportModel.revertHint).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
        case .reverting:
            busy("Reverting the import\u{2026}")
        case .reverted(let r):
            Text(r.message).font(DS.Font.label).foregroundStyle(DS.text)
        case .failed(let message):
            HStack(alignment: .firstTextBaseline, spacing: DS.Space.s) {
                Image(systemName: "xmark.octagon").foregroundStyle(DS.reject).accessibilityHidden(true)
                Text(message).font(DS.Font.label).foregroundStyle(DS.text)
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Failed: \(message)")
            if let warnings = model.info?.warnings, !warnings.isEmpty {
                ForEach(Array(warnings.enumerated()), id: \.offset) { _, w in
                    Text(w).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                }
            }
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
            group("Library", AppleImportCategories.organise)
            group("Edits", AppleImportCategories.develop)
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
            case .loading, .previewing, .importing, .reverting:
                Button("Cancel", role: .cancel) { close() }.disabled(model.phase == .importing || model.phase == .reverting)
            case .done:
                if model.canRevert {
                    Button("Revert Import") { Task { await model.revert() } }
                        .accessibilityHint("Puts back what the Aperture and iPhoto imports since the last revert changed")
                }
                Button("Done") { close() }.keyboardShortcut(.defaultAction)
            case .reverted, .failed:
                if case .failed = model.phase {
                    if model.canRevert {
                        Button("Revert Import") { Task { await model.revert() } }
                            .accessibilityHint("Puts back what the failed import had already changed")
                    }
                    if model.session == nil, let originals = model.originalsFolder {
                        Button("Open \u{201c}\(originals.lastPathComponent)\u{201d} as Session\u{2026}") {
                            Task { await model.openAsSession(originals.path) }
                        }
                        .accessibilityHint("Opens the library's originals in place; the library itself is never written to")
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

/// File ▸ Revert Aperture or iPhoto Import asks first.
struct AppleLibraryRevertDialog: ViewModifier {
    @Bindable var model: AppModel

    func body(content: Content) -> some View {
        content.confirmationDialog("Revert Aperture or iPhoto Import?", isPresented: $model.confirmingAppleRevert,
                                   titleVisibility: .visible) {
            Button("Revert", role: .destructive) { Task { await model.revertAppleLibraryImport() } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Takes back every Aperture and iPhoto import since the last revert. Changes you made in Darkslide afterwards are kept.")
        }
    }
}
