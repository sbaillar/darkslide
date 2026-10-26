import AppKit
import LuxtableCore
import LuxtableUI
import SwiftUI

/// Settings › Archive: the destination (a folder through rsync, or S3-compatible object
/// storage), interval, on/off. (Amazon Photos has its own tab.)
struct ArchiveSettingsView: View {
    @State var model: ArchiveSettingsModel
    /// The target being typed: committed on Return or when the field loses
    /// focus, never per keystroke (A-H4).
    @State private var draftTarget = ""
    @FocusState private var editingTarget: Bool

    var body: some View {
        @Bindable var settings = model.settings
        Form {
            Section("Archive") {
                Toggle("Archive sessions in the background", isOn: $settings.archiveEnabled)
                Picker("Destination", selection: $model.destination) {
                    ForEach(ArchiveDestinationKind.allCases, id: \.self) { Text($0.title).tag($0) }
                }
                .pickerStyle(.segmented)
                Text(ArchiveSettingsModel.switchNote).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
            if model.destination == .folder { folderSection } else { objectSection }
            Section("Schedule") {
                Picker("Check every", selection: $settings.archiveIntervalMinutes) {
                    ForEach(ArchiveSettingsModel.intervalChoices, id: \.self) { Text("\($0) min").tag($0) }
                }
                Text("The archive runs when there are changes, after an import, and when you close a session. "
                    + "It never deletes anything from the archive.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Toggle("Archive rejected photos", isOn: $settings.archiveRejected) // N2: the archive is the backup
                    .help("Off: photos you rejected (and the session's Trash) stay out of the archive, "
                          + "and deleting them leaves no copy")
            }
        }
        .formStyle(.grouped)
        .frame(width: 520)
        .padding(DS.Space.m)
        .task {
            draftTarget = settings.archiveTarget
            await model.refresh()
        }
        .onChange(of: settings.archiveTarget) { _, committed in
            if !editingTarget { draftTarget = committed }
        }
    }

    /// rsync into a folder: a mounted volume or a local folder.
    private var folderSection: some View {
        Section("Folder") {
            HStack {
                TextField("Archive folder", text: $draftTarget, prompt: Text("/Volumes/NAS/Photos"))
                    .focused($editingTarget)
                    .onSubmit { Task { await model.commitTarget(draftTarget) } }
                    .onChange(of: editingTarget) { _, editing in
                        if !editing { Task { await model.commitTarget(draftTarget) } }
                    }
                Button("Choose…") {
                    if let url = pickFolder("Choose the archive folder (on your NAS, an external disk or this Mac)") {
                        Task { await model.setTarget(url.path) }
                    }
                }
            }
            if let s = model.suggestion {
                Button("Use \(s)") { Task { await model.useSuggestion() } }
                    .buttonStyle(.link)
                    .help("Your pics share is mounted")
            }
            Text(model.targetText).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            if let kind = model.folderKindText {
                Text(kind).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
            if model.canCreateTarget {
                Button("Create It") { Task { await model.createTarget() } }
                    .help("Create this folder now (the archive also creates a missing folder on a mounted volume "
                          + "on its first run, but only on the volume you confirmed)")
            }
            if let e = model.createError {
                Text(e).font(DS.Font.caption).foregroundStyle(DS.warningText)
            }
        }
    }

    /// S3-compatible object storage: where, and the keys (Keychain only).
    private var objectSection: some View {
        Section("Object storage") {
            TextField("Endpoint", text: $model.objectDraft.endpoint,
                      prompt: Text("https://s3.eu-central-003.backblazeb2.com (empty: AWS)"))
                .help("The service's S3 address. Leave empty for Amazon S3: the region picks it.")
            TextField("Region", text: $model.objectDraft.region, prompt: Text("us-east-1 (Cloudflare R2: auto)"))
            TextField("Bucket", text: $model.objectDraft.bucket, prompt: Text("my-photo-archive"))
            TextField("Path", text: $model.objectDraft.prefix, prompt: Text("luxtable/"))
                .help("Key prefix: each session goes to <path><session name>/…")
            Toggle("Path-style addressing", isOn: $model.objectDraft.pathStyle)
                .help("https://host/bucket/key instead of https://bucket.host/key: MinIO and some other services need it")
            Text(model.objectLayoutText).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            if let problem = model.objectProblem {
                Text(problem.prefix(1).uppercased() + problem.dropFirst() + ".")
                    .font(DS.Font.caption).foregroundStyle(DS.warningText)
            }
            TextField("Access key ID", text: $model.accessKeyIDDraft)
                .accessibilityLabel("Object storage access key ID")
            SecureField("Secret access key", text: $model.secretDraft,
                        prompt: Text(model.hasSavedSecret ? "Saved in your Keychain" : "Secret access key"))
                .accessibilityLabel("Object storage secret access key")
            HStack {
                Button("Save") { model.saveObjectStore() }
                Button("Test Connection") { Task { await model.testConnection() } }.disabled(model.testingConnection)
                if model.testingConnection { ProgressView().controlSize(.small) }
                Spacer()
                if model.hasSavedSecret { Button("Remove Keys") { model.removeKeys() } }
            }
            if let m = model.objectMessage {
                Text(m).font(DS.Font.caption).foregroundStyle(model.objectFailed ? DS.warningText : DS.textSecondary)
            }
            Text("Test Connection checks the bucket and leaves one small test object under the path, in "
                + ".lt-archive-test/.")
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            Text(ArchiveSettingsModel.objectNote).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
        }
    }

    private func pickFolder(_ message: String) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.message = message
        return panel.runModal() == .OK ? panel.url : nil
    }
}
