import AppKit
import SwiftUI

/// Settings › Archive: rsync target, interval, on/off; Amazon Photos folder.
struct ArchiveSettingsView: View {
    @State var model: ArchiveSettingsModel
    /// The target being typed: committed on Return or when the field loses
    /// focus, never per keystroke (A-H4).
    @State private var draftTarget = ""
    @FocusState private var editingTarget: Bool

    var body: some View {
        @Bindable var settings = model.settings
        Form {
            Section("Archive (rsync)") {
                Toggle("Archive sessions in the background", isOn: $settings.archiveEnabled)
                HStack {
                    TextField("Archive target", text: $draftTarget, prompt: Text("/Volumes/NAS/Photos"))
                        .focused($editingTarget)
                        .onSubmit { Task { await model.commitTarget(draftTarget) } }
                        .onChange(of: editingTarget) { _, editing in
                            if !editing { Task { await model.commitTarget(draftTarget) } }
                        }
                    Button("Choose…") {
                        if let url = pickFolder("Choose the archive folder (e.g. on your NAS)") {
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
                if model.canCreateTarget {
                    Button("Create It") { Task { await model.createTarget() } }
                        .help("Create this folder now (the archive also creates it on its first run, but only on the volume you confirmed)")
                }
                if let e = model.createError {
                    Text(e).font(DS.Font.caption).foregroundStyle(DS.warningText)
                }
                Picker("Check every", selection: $settings.archiveIntervalMinutes) {
                    ForEach(ArchiveSettingsModel.intervalChoices, id: \.self) { Text("\($0) min").tag($0) }
                }
                Text("rsync runs when there are changes, after an import, and when you close a session. It never deletes from the archive.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Toggle("Archive rejected photos", isOn: $settings.archiveRejected) // N2: the archive is the backup
                    .help("Off: photos you rejected (and the session's Trash) stay out of the archive, "
                          + "and deleting them leaves no copy")
            }
            Section("Amazon Photos") {
                LabeledContent("Backed-up folder", value: settings.amazonPhotosFolder?.path ?? "Off")
                HStack {
                    Button("Choose…") {
                        if let url = pickFolder("Choose the folder the Amazon Photos app backs up") {
                            settings.chooseAmazonFolder(url)
                            Task { await model.refresh() }
                        }
                    }
                    if settings.amazonPhotosFolder != nil {
                        Button("Turn Off") { settings.chooseAmazonFolder(nil) }
                    }
                }
                Text(model.amazonText).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Text(ArchiveSettingsModel.amazonVideosNote).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Toggle("Send rejected photos to Amazon Photos", isOn: $settings.amazonRejected) // N2
                    .help("Off: photos you rejected aren't handed to Amazon Photos")
                if model.amazonAppInstalled == false {
                    Link("Get the Amazon Photos app", destination: ArchiveSettingsModel.amazonInstallURL)
                }
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

    private func pickFolder(_ message: String) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.message = message
        return panel.runModal() == .OK ? panel.url : nil
    }
}
