import SwiftUI

/// Capture One import: the dry run's preview, then the import, then the result
/// (with a revert). Darkslide edits that would be replaced are listed, and are
/// only overwritten when the user turns that on.
struct CaptureOneImportSheet: View {
    let app: AppModel
    @Bindable var model: CaptureOneImportModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text(model.title).font(.headline).foregroundStyle(DS.text)
                .accessibilityAddTraits(.isHeader)
            content
            buttons
        }
        .padding(DS.Space.l)
        .frame(width: 540)
        .background(DS.panelBackground)
    }

    @ViewBuilder private var content: some View {
        switch model.phase {
        case .previewing:
            busy("Reading the Capture One session (nothing is changed)\u{2026}")
        case .preview(let s):
            if model.offered {
                Text("Bring its ratings, colour tags, keywords, Selects and edits into Darkslide? The Capture One folder is never changed.")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
            }
            summary(s)
            if !s.conflicts.isEmpty {
                Toggle("Overwrite my Darkslide edits on conflicts", isOn: $model.overwriteEdits)
                    .font(DS.Font.label).foregroundStyle(DS.text)
                    .accessibilityHint("Off: photos you changed in Darkslide since the last import keep your values")
            }
        case .importing:
            busy("Importing\u{2026} The Capture One folder is only read.")
        case .done(let s):
            summary(s)
            if model.canRevert {
                Text(CaptureOneImportModel.revertHint)
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
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

    private func summary(_ s: C1ImportSummary) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: DS.Space.s) {
                section(nil, s.lines)
                section("Edits by tool", s.toolLines)
                section("Conflicts: changed in Darkslide since the last import", s.conflictLines, warning: true)
                section("Not mapped (no Darkslide equivalent)", s.notMappedLines)
                section("Left out", s.leftOutLines)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxHeight: 340)
    }

    @ViewBuilder private func section(_ title: String?, _ lines: [String], warning: Bool = false) -> some View {
        if !lines.isEmpty {
            VStack(alignment: .leading, spacing: DS.Space.xxs) {
                if let title {
                    Text(title.uppercased()).font(DS.Font.caption).tracking(0.5)
                        .foregroundStyle(warning ? DS.warningText : DS.textSecondary)
                        .accessibilityLabel(title)
                        .accessibilityAddTraits(.isHeader)
                }
                ForEach(Array(lines.enumerated()), id: \.offset) { _, l in
                    Text(l).font(DS.Font.label).foregroundStyle(DS.text).textSelection(.enabled)
                }
            }
        }
    }

    @ViewBuilder private var buttons: some View {
        HStack {
            Spacer()
            switch model.phase {
            case .preview:
                Button(model.offered ? "Not Now" : "Cancel", role: .cancel) { close() }.keyboardShortcut(.cancelAction)
                Button(model.overwriteEdits ? "Import and Overwrite" : "Import") { Task { await model.run() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!model.canImport)
            case .previewing, .importing, .reverting:
                Button("Cancel", role: .cancel) { close() }.disabled(model.phase != .previewing)
            case .done:
                if model.canRevert {
                    Button("Revert Import") { Task { await model.revert() } }
                        .accessibilityHint("Puts back what the Capture One imports since the last revert changed")
                }
                Button("Done") { close() }.keyboardShortcut(.defaultAction)
            case .reverted, .failed:
                Button("Done") { close() }.keyboardShortcut(.defaultAction)
            }
        }
    }

    private func close() {
        app.sheet = nil
    }
}

/// File ▸ Revert Capture One Import asks first.
struct CaptureOneRevertDialog: ViewModifier {
    @Bindable var model: AppModel

    func body(content: Content) -> some View {
        content.confirmationDialog("Revert Capture One Import?", isPresented: $model.confirmingCaptureOneRevert,
                                   titleVisibility: .visible) {
            Button("Revert", role: .destructive) { Task { await model.revertCaptureOneImport() } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Takes back every Capture One import since the last revert. Changes you made in Darkslide afterwards are kept.")
        }
    }
}
