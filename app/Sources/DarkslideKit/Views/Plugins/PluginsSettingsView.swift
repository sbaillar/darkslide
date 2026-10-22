import AppKit
import DarkslideFFI
import SwiftUI
import UniformTypeIdentifiers

/// Settings ▸ Plugins: what is installed, what each one waits for, Install…, Remove.
struct PluginsSettingsView: View {
    @Bindable var store: PluginStore
    /// A package chosen or dropped, staged and waiting for the user to confirm.
    @State private var pending: PluginInstallPreview?

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Text("Plug-ins add looks, styles, voice phrases and export steps. Install a .dsplugin folder, or drop one here.")
                .font(DS.Font.label).foregroundStyle(DS.textSecondary)
            List(store.plugins, id: \.path) { PluginRow(record: $0, store: store) }
                .frame(minHeight: 260)
                .dropDestination(for: URL.self) { urls, _ in
                    guard let url = urls.first else { return false }
                    choose(url)
                    return true
                }
            if let e = store.lastError {
                Label(e, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.warningText)
            }
            ForEach(store.contentErrors, id: \.self) { e in
                Label(e, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.warningText)
            }
            HStack {
                Button("Install…", action: pick)
                Button("Reveal Plugins Folder") {
                    try? FileManager.default.createDirectory(at: AppPaths.plugins, withIntermediateDirectories: true)
                    NSWorkspace.shared.activateFileViewerSelecting([AppPaths.plugins])
                }
                Button("Reload") { Task { await store.reload() } }
                Button("Add Development Folder…", action: pickDev)
                    .help("Load a plug-in you are writing from its folder; it reloads without re-approval")
                Spacer()
                Toggle("Allow unsigned code plug-ins", isOn: Binding(get: { store.allowUnsignedCode },
                                                                     set: { on in Task { await store.setAllowUnsignedCode(on) } }))
                    .help("For plug-in authors: run code plug-ins that aren't signed by someone you trust")
            }
        }
        .padding()
        .sheet(isPresented: Binding(get: { pending != nil }, set: { if !$0 { pending = nil } })) {
            if let p = pending {
                InstallSheet(preview: p) {
                    Task { await store.install(p) }
                    pending = nil
                } cancel: {
                    Task { await store.cancel(p) }
                    pending = nil
                }
            }
        }
    }

    private func pick() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = true
        panel.treatsFilePackagesAsDirectories = false
        panel.prompt = "Install"
        if panel.runModal() == .OK, let url = panel.url { choose(url) }
    }

    private func choose(_ url: URL) {
        Task { if let preview = await store.preview(url) { pending = preview } }
    }

    private func pickDev() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.treatsFilePackagesAsDirectories = false
        panel.prompt = "Load"
        if panel.runModal() == .OK, let url = panel.url { Task { await store.addDevFolder(url) } }
    }
}

private struct PluginRow: View {
    let record: PluginRecord
    let store: PluginStore

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                VStack(alignment: .leading) {
                    Text("“\(record.name)” \(record.version ?? "")").font(DS.Font.label.weight(.semibold))
                    Text([record.trustLabel, record.contributionSummary, record.stateLabel]
                        .filter { !$0.isEmpty }.joined(separator: " · "))
                        .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                }
                Spacer()
                if record.state != .error, record.state != .shadowed {
                    Toggle("Enabled", isOn: Binding(get: { record.state != .disabled },
                                                    set: { on in Task { await store.setEnabled(record.id, on) } }))
                        .labelsHidden().toggleStyle(.switch)
                }
                if record.dev, let path = store.devFolders.first(where: { record.path.hasPrefix($0) }) {
                    Button("Unlink") { Task { await store.removeDevFolder(path) } }.help("Stop loading this development folder")
                } else if !record.builtIn {
                    Button("Remove") { Task { await store.remove(record.id) } }.help("Move it to the Trash")
                }
            }
            if !record.summary.isEmpty { Text("“\(record.summary)”").font(DS.Font.caption) }
            if let m = record.message { Text(m).font(DS.Font.caption).foregroundStyle(DS.warningText) }
            ForEach(record.approvals, id: \.title) { a in
                HStack {
                    Text(a.title).font(DS.Font.caption)
                    Spacer()
                    Button(a.action) {
                        if a.needsFolder {
                            if let folder = Self.chooseFolder() { Task { await store.approve(record, a, folder: folder) } }
                        } else {
                            Task { await store.approve(record, a) }
                        }
                    }
                    .controlSize(.small)
                }
            }
            ForEach(record.permissions.filter(\.granted), id: \.permission) { p in
                HStack {
                    Label(p.label, systemImage: p.sensitive ? "lock.open" : "checkmark").font(DS.Font.caption)
                    Spacer()
                    if !record.builtIn { Button("Revoke") { Task { await store.revoke(record.id, p.permission) } }.controlSize(.small) }
                }
            }
        }
        .padding(.vertical, 2)
    }

    static func chooseFolder() -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Allow This Folder"
        panel.message = "Choose the one folder this plug-in may use."
        return panel.runModal() == .OK ? panel.url : nil
    }
}

/// The confirmation before an install: who made it, what it adds and asks for.
private struct InstallSheet: View {
    let preview: PluginInstallPreview
    let install: () -> Void
    let cancel: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Text("Install a plug-in").font(.headline)
            Text("“\(preview.name)” version \(preview.version)").font(DS.Font.label.weight(.semibold))
            Text(PluginInstallText.signer(preview)).font(DS.Font.label)
            if !preview.summary.isEmpty { Text("It says: “\(preview.summary)”").font(DS.Font.caption) }
            Text(PluginInstallText.adds(preview)).font(DS.Font.caption)
            ForEach(preview.permissions, id: \.permission) { p in
                Label(p.label, systemImage: p.sensitive ? "exclamationmark.shield" : "info.circle").font(DS.Font.caption)
            }
            if let note = PluginInstallText.replaces(preview) {
                Text(note).font(DS.Font.caption).foregroundStyle(preview.downgrade ? DS.warningText : DS.textSecondary)
            }
            Text("Nothing is granted until you approve it in Settings ▸ Plugins.")
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            HStack {
                Spacer()
                Button("Cancel", action: cancel).keyboardShortcut(.cancelAction)
                Button(preview.downgrade ? "Install Older Version" : "Install") { install() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding()
        .frame(width: 420)
    }
}
