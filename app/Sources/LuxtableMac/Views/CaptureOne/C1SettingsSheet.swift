import LuxtableCore
import LuxtableUI
import SwiftUI
import UniformTypeIdentifiers

/// File ▸ Import Capture One Settings…: what the folder holds per category (with
/// checkboxes) and what the import would do with each item, then the import.
struct C1SettingsSheet: View {
    let app: AppModel
    @Bindable var model: C1SettingsImportModel
    @State private var choosing = false

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text("Import Capture One Settings").font(.headline).foregroundStyle(DS.text)
                .accessibilityAddTraits(.isHeader)
            folderRow
            content
            buttons
        }
        .padding(DS.Space.l)
        .frame(width: 600)
        .background(DS.panelBackground)
        .fileImporter(isPresented: $choosing, allowedContentTypes: [.folder]) { result in
            if case .success(let url) = result { Task { await model.choose(folder: url) } }
        }
    }

    private var folderRow: some View {
        HStack(spacing: DS.Space.s) {
            Text((model.folder.path as NSString).abbreviatingWithTildeInPath)
                .font(DS.Font.label).foregroundStyle(DS.textSecondary).lineLimit(1).truncationMode(.middle)
                .accessibilityLabel("Folder \(model.folder.lastPathComponent)")
            Spacer()
            Button("Choose Folder\u{2026}") { choosing = true }
                .disabled(model.phase == .scanning || model.phase == .importing)
        }
    }

    @ViewBuilder private var content: some View {
        switch model.phase {
        case .scanning:
            busy("Reading Capture One's settings (nothing is changed)\u{2026}")
        case .preview(let plan):
            ScrollView {
                VStack(alignment: .leading, spacing: DS.Space.m) {
                    ForEach(C1SettingsCategory.allCases, id: \.self) { category(plan, $0) }
                    if let scan = model.scan { found(scan) }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 420)
        case .importing:
            busy("Importing\u{2026} The Capture One folder is only read.")
        case .done(let plan):
            ForEach(model.doneLines(plan), id: \.self) { Text($0).font(DS.Font.label).foregroundStyle(DS.text) }
            if model.canRevert {
                Text(C1SettingsImportModel.revertHint).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
        case .reverted(let r):
            Text(r.message).font(DS.Font.label).foregroundStyle(DS.text)
        case .failed(let message):
            Label(message, systemImage: "xmark.octagon").font(DS.Font.label).foregroundStyle(DS.text)
                .accessibilityLabel("Failed: \(message)")
        }
    }

    private func busy(_ text: String) -> some View {
        HStack(spacing: DS.Space.s) {
            ProgressView().controlSize(.small)
            Text(text).font(DS.Font.label).foregroundStyle(DS.text)
        }
        .accessibilityElement(children: .combine)
    }

    private func ticked(_ c: C1SettingsCategory) -> Binding<Bool> {
        Binding(get: { model.categories.contains(c) },
                set: { if $0 { model.categories.insert(c) } else { model.categories.remove(c) } })
    }

    @ViewBuilder private func category(_ plan: C1SettingsPlan, _ c: C1SettingsCategory) -> some View {
        let rows = plan.rows(in: c)
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            Toggle(isOn: ticked(c)) {
                Text(c.title).font(DS.Font.label.weight(.semibold)).foregroundStyle(DS.text)
            }
            .disabled(plan.changes(in: [c]) == 0)
            .accessibilityValue(plan.summary(c))
            Text(plan.summary(c)).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            if !rows.isEmpty {
                DisclosureGroup("Details") {
                    VStack(alignment: .leading, spacing: DS.Space.xxs) {
                        ForEach(Array(rows.enumerated()), id: \.offset) { _, row in itemRow(row) }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .font(DS.Font.caption)
            }
        }
    }

    private func itemRow(_ row: C1SettingsPlan.Row) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(alignment: .firstTextBaseline) {
                Text(row.item.group.map { "\($0) / \(row.item.title)" } ?? row.item.title)
                    .font(DS.Font.label).foregroundStyle(DS.text).textSelection(.enabled)
                Spacer()
                Text(row.action.label).font(DS.Font.caption)
                    .foregroundStyle(row.action.writes ? DS.text : DS.textSecondary)
            }
            ForEach(Array(row.item.notes.enumerated()), id: \.offset) { _, n in
                Text("\(n.dropped ? "Not carried over" : "Approximate"): \(n.text)")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary).textSelection(.enabled)
                    .padding(.leading, DS.Space.m)
            }
        }
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder private func found(_ scan: C1SettingsScan) -> some View {
        let lines = scan.folders.map { "\($0.name): \($0.files == 0 ? "empty" : "\($0.files) file\($0.files == 1 ? "" : "s")")" }
        section("Found", lines)
        section("Preferences not carried over", scan.notes.map(\.text))
        section("Not read", scan.skipped.map { "\($0.file): \($0.reason)" })
    }

    @ViewBuilder private func section(_ title: String, _ lines: [String]) -> some View {
        if !lines.isEmpty {
            VStack(alignment: .leading, spacing: DS.Space.xxs) {
                Text(title.uppercased()).font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
                    .accessibilityLabel(title).accessibilityAddTraits(.isHeader)
                ForEach(Array(lines.enumerated()), id: \.offset) { _, l in
                    Text(l).font(DS.Font.caption).foregroundStyle(DS.text).textSelection(.enabled)
                }
            }
        }
    }

    @ViewBuilder private var buttons: some View {
        HStack {
            if model.canRevert, model.phase != .importing, model.phase != .scanning {
                Button("Revert Last Import") { model.revert() }
                    .help(C1SettingsImportModel.revertHint)
            }
            Spacer()
            switch model.phase {
            case .preview:
                Button("Cancel", role: .cancel) { close() }.keyboardShortcut(.cancelAction)
                Button("Import") { Task { await model.run() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!model.canImport)
            case .scanning, .importing:
                Button("Cancel", role: .cancel) { close() }.disabled(model.phase == .importing)
            case .done, .reverted, .failed:
                Button("Done") { close() }.keyboardShortcut(.defaultAction)
            }
        }
    }

    private func close() { app.sheet = nil }
}
