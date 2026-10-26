import LuxtableCore
import LuxtableUI
import SwiftUI

/// Settings ▸ Immich: the server and API key, where Immich sees the archive, the
/// developed copies and sidecars, and the album rules.
struct ImmichSettingsView: View {
    @Bindable var model: ImmichModel
    let archiveTarget: String
    /// The archive goes to object storage: Immich has no folder to index.
    var objectStorage = false
    @State private var rules: [ImmichAlbumRule] = []
    @State private var xmpSidecars = true
    @State private var labelTags = true
    @State private var quality = 92
    @State private var longEdge = 0

    var body: some View {
        Form {
            if objectStorage {
                Section {
                    Label(ImmichModel.objectStorageNote, systemImage: "exclamationmark.triangle")
                        .font(DS.Font.caption).foregroundStyle(DS.warningText)
                }
            }
            Section("Server") {
                TextField("Server address", text: $model.serverEntry, prompt: Text("https://immich.example.org"))
                    .accessibilityLabel("Immich server address")
                if model.hasKey {
                    HStack {
                        Label("API key saved in your Keychain", systemImage: "key.fill")
                        Spacer()
                        Button("Remove Key") { Task { await model.removeKey() } }
                    }
                } else {
                    SecureField("API key", text: $model.keyEntry)
                        .accessibilityLabel("Immich API key")
                }
                HStack {
                    Button("Save") { Task { await model.save() } }.disabled(!model.canSave)
                    Button("Test Connection") { Task { await model.testConnection() } }.disabled(model.busy)
                    if model.busy { ProgressView().controlSize(.small) }
                }
                if let m = model.message { Text(m).font(DS.Font.caption).foregroundStyle(DS.textSecondary) }
                Text("Create the key in Immich under Account Settings ▸ API Keys. Luxtable only creates albums, changes "
                    + "their photos and asks for library scans; it never uploads or deletes anything.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
            Section("Where Immich sees the archive") {
                TextField("Folder on the server", text: $model.pathOnServerEntry, prompt: Text("/mnt/photos"))
                TextField("Same folder on this Mac", text: $model.pathOnMacEntry, prompt: Text("/Volumes/photos"))
                HStack {
                    Button("Detect") { Task { await model.detectMapping() } }.disabled(model.busy)
                    Button("Save") { Task { await model.save() } }.disabled(!model.canSave)
                }
                Text(archiveTarget.isEmpty ? "Set the archive folder in Settings ▸ Archive first."
                    : "The archive goes to \(archiveTarget). Immich indexes it as an external library (read-only is fine).")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
            Section("Developed copies and sidecars") {
                Text("Choose photos in the browser, then Photo ▸ Keep a Developed Copy for Immich: a JPEG with every "
                    + "edit goes to the session's Developed folder and is kept up to date.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Stepper("JPEG quality: \(quality)", value: $quality, in: 50...100)
                    .onChange(of: quality) { model.settings.quality = quality; model.settingsChanged() }
                Picker("Size", selection: $longEdge) {
                    Text("Full size").tag(0)
                    Text("Long edge 4096 px").tag(4096)
                    Text("Long edge 2560 px").tag(2560)
                }
                .onChange(of: longEdge) { model.settings.longEdge = longEdge; model.settingsChanged() }
                Toggle("XMP sidecars next to the originals", isOn: $xmpSidecars)
                    .onChange(of: xmpSidecars) { model.settings.xmpSidecars = xmpSidecars; model.settingsChanged() }
                Text("IMG_0001.ARW.xmp with stars, title, caption, keywords and position, so RAWs show them in Immich "
                    + "too. Sidecars from Lightroom or Capture One are never touched.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Toggle("Colour label and pick as tags", isOn: $labelTags)
                    .onChange(of: labelTags) { model.settings.labelTags = labelTags; model.settingsChanged() }
            }
            Section("Albums") {
                ForEach($rules) { $rule in
                    ImmichRuleRow(rule: $rule) {
                        rules.removeAll { $0.id == rule.id }
                    }
                }
                HStack {
                    Button("Add Album") {
                        rules.append(ImmichAlbumRule(album: "Luxtable Picks", pickedOnly: true))
                    }
                    Spacer()
                    Button("Sync Albums Now") { Task { await model.syncAlbums(every: true) } }
                        .disabled(!model.canSync)
                    if model.syncing { ProgressView().controlSize(.small) }
                }
                if let s = model.lastSync {
                    Text(s).font(DS.Font.caption)
                        .foregroundStyle(model.lastSyncFailed ? DS.reject : DS.textSecondary)
                }
                Text("Albums follow every archived session and are synced after each archive run. Luxtable adds the "
                    + "developed copy when there is one, else the original, and only removes photos it added itself.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
        }
        .formStyle(.grouped)
        .onAppear {
            model.loadKeyState()
            rules = model.settings.rules
            xmpSidecars = model.settings.xmpSidecars
            labelTags = model.settings.labelTags
            quality = model.settings.quality
            longEdge = model.settings.longEdge
        }
        .onChange(of: rules) { model.settings.rules = rules }
    }
}

/// One album rule: its album name and what it takes.
struct ImmichRuleRow: View {
    @Binding var rule: ImmichAlbumRule
    let remove: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack {
                TextField("Album", text: $rule.album).accessibilityLabel("Immich album name")
                Button(role: .destructive, action: remove) { Image(systemName: "minus.circle") }
                    .buttonStyle(.borderless)
                    .help("Stop syncing this album (it stays in Immich)")
            }
            HStack {
                Picker("Stars", selection: $rule.minRating) {
                    Text("Any").tag(0)
                    ForEach(1...5, id: \.self) { n in Text(String(repeating: "★", count: n) + (n < 5 ? "+" : "")).tag(n) }
                }
                Toggle("Picked", isOn: $rule.pickedOnly)
                Picker("Label", selection: $rule.label) {
                    Text("Any").tag(ColorTag?.none)
                    ForEach(ColorTag.allCases.filter { $0 != .none }, id: \.self) { Text($0.name).tag(ColorTag?.some($0)) }
                }
                TextField("Keyword", text: $rule.keyword)
            }
            Text(rule.summary).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
        }
    }
}

/// Status bar: the last album sync, while Immich is set up.
struct ImmichStatusLabel: View {
    let immich: ImmichModel

    var body: some View {
        if immich.isSetUp, let s = immich.syncing ? "Immich: syncing albums…" : immich.lastSync {
            Label(s, systemImage: "photo.stack")
                .foregroundStyle(immich.lastSyncFailed ? DS.reject : DS.textSecondary)
                .help("Albums in Immich (Settings ▸ Immich)")
                .accessibilityLabel(s)
        }
    }
}
