import AppKit
import LuxtableCore
import LuxtableUI
import SwiftUI

/// First-run sheet. Four short steps, sensible defaults, "Skip" always available.
struct OnboardingView: View {
    @Bindable var model: AppModel
    @State private var onboarding: OnboardingModel
    @State private var error: String?

    init(model: AppModel) {
        self.model = model
        _onboarding = State(initialValue: OnboardingModel(settings: model.settings, features: model.features))
    }

    var body: some View {
        VStack(spacing: 0) {
            ProgressView(value: onboarding.progress)
                .progressViewStyle(.linear)
                .tint(DS.accent)
            VStack(alignment: .leading, spacing: DS.Space.l) {
                Text(onboarding.step.title).font(.system(size: 22, weight: .semibold))
                content
                    .frame(maxWidth: .infinity, alignment: .leading)
                Spacer(minLength: 0)
                if let error {
                    Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(DS.reject).font(DS.Font.label)
                }
            }
            .padding(DS.Space.xl)
            .animation(.snappy, value: onboarding.step)
            Divider()
            HStack {
                if !onboarding.isFirst {
                    Button("Back", action: onboarding.back)
                }
                Spacer()
                Button("Skip for Now") {
                    model.settings.onboardingComplete = true
                    model.sheet = nil
                }
                .help("Use the defaults; change them later in Settings (⌘,)")
                Button(onboarding.isLast ? "Start Using Luxtable" : "Continue") {
                    if onboarding.isLast { finish() } else { onboarding.next() }
                }
                .keyboardShortcut(.defaultAction)
                .buttonStyle(.borderedProminent)
                .tint(DS.accent)
                .disabled(onboarding.isLast && !onboarding.canFinish)
            }
            .padding(DS.Space.m)
        }
        .frame(width: 600, height: 460)
        .task { await onboarding.suggestArchiveTarget() }
        .background(DS.panelBackground)
    }

    @ViewBuilder private var content: some View {
        switch onboarding.step {
        case .welcome:
            VStack(alignment: .leading, spacing: DS.Space.m) {
                let imp = ImportWording.onboardingImport(verifies: model.importer is any ImportVerifying)
                feature("sdcard", imp.title, imp.text)
                feature("keyboard", "Cull at the speed of your fingers", "Arrows to move, 1–5 to rate, P/X to pick or reject. Press ? any time.")
                feature("sparkles", "Let AI do the first pass", "AI Cull flags closed eyes and soft focus on this Mac. Nothing is uploaded.")
                feature("text.bubble", "Edit with words", "Press ⌘K and type “warmer and a bit brighter”. Only what you name changes.") // [nl9]
                feature("externaldrive.badge.checkmark", "Archived in the background",
                        model.archive is UnavailableArchiveService
                            ? "Archiving isn't available in this mode."
                            : model.features.isOn(.archive) ? "Sessions sync to your NAS while you work."
                            : "If you like, sessions sync to your NAS while you work (next steps).")
            }
        case .familiar:
            VStack(alignment: .leading, spacing: DS.Space.m) {
                Text("Luxtable keeps its own look. This only moves panels, keys and a few names to where your hands expect them. Change it any time in Settings › Familiar.")
                    .foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
                FamiliarChoiceList(selection: $onboarding.familiar)
            }
        case .sessions:
            VStack(alignment: .leading, spacing: DS.Space.m) {
                Text("Each import becomes a session: a plain folder with your originals and edits. Keep it on your fastest disk.")
                    .foregroundStyle(DS.textSecondary)
                folderRow(onboarding.sessionsFolder.path, symbol: "folder") {
                    if let url = pickFolder(message: "Choose where Luxtable keeps sessions") { onboarding.sessionsFolder = url }
                }
            }
        case .archive:
            VStack(alignment: .leading, spacing: DS.Space.m) {
                Text("Luxtable can copy sessions to another disk or a NAS in the background with rsync. It never deletes anything in the archive. Optional: turn it on or off any time in Settings ▸ Plugins.")
                    .foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
                Toggle("Archive sessions automatically", isOn: $onboarding.archiveEnabled)
                if onboarding.archiveEnabled {
                    HStack {
                        TextField("Archive folder", text: $onboarding.archiveTarget, prompt: Text("Choose… or type a path"))
                            .textFieldStyle(.roundedBorder)
                        Button("Choose…") {
                            if let url = pickFolder(message: "Choose the archive folder") { onboarding.archiveTarget = url.path }
                        }
                    }
                    archiveStatus
                        .task(id: onboarding.archiveTarget) { await onboarding.refreshArchiveStatus() }
                }
            }
        case .amazon:
            VStack(alignment: .leading, spacing: DS.Space.m) {
                Text("Amazon has no public Photos API. Luxtable mirrors your originals into a folder, and the official Amazon Photos app backs that folder up.")
                    .foregroundStyle(DS.textSecondary)
                folderRow(onboarding.amazonFolder?.path ?? "Off", symbol: "icloud.and.arrow.up") {
                    onboarding.amazonFolder = pickFolder(message: "Choose the folder the Amazon Photos app backs up")
                }
                if onboarding.amazonFolder != nil {
                    Button("Turn Off") { onboarding.amazonFolder = nil }.buttonStyle(.link)
                }
            }
        }
    }

    @ViewBuilder private var archiveStatus: some View {
        switch onboarding.archiveStatus {
        case .checking:
            Label("Checking…", systemImage: "clock").foregroundStyle(DS.textSecondary)
        case .unreachable:
            Label("The target didn't respond. It may be a slow or offline network share; syncs wait until it answers.",
                  systemImage: "wifi.exclamationmark").foregroundStyle(DS.textSecondary)
        case .empty:
            Label("Choose a folder, for example on your NAS.", systemImage: "folder.badge.questionmark")
                .foregroundStyle(DS.textSecondary)
        case .ready:
            Label("Ready", systemImage: "checkmark.circle.fill").foregroundStyle(DS.pick)
        case .notMounted(let share):
            Label("“\(share)” isn't mounted right now. Syncs wait until it is.", systemImage: "clock")
                .foregroundStyle(DS.textSecondary)
        case .creatable:
            HStack {
                Label("This folder doesn't exist yet. Luxtable creates it when you finish.", systemImage: "folder.badge.plus")
                    .foregroundStyle(DS.textSecondary)
                Button("Create It Now") {
                    Task {
                        do { try await onboarding.createArchiveTarget() } catch { self.error = error.localizedDescription }
                    }
                }
            }
        case .missing:
            Label("Neither this folder nor the folder it's in exists, or it can't be written. Choose another folder.",
                  systemImage: "exclamationmark.triangle").foregroundStyle(DS.warningText)
        case .invalid(let why):
            Label(why, systemImage: "exclamationmark.triangle").foregroundStyle(DS.warningText)
        }
    }

    private func feature(_ symbol: String, _ title: String, _ text: String) -> some View {
        HStack(alignment: .top, spacing: DS.Space.m) {
            Image(systemName: symbol).font(.system(size: 18)).foregroundStyle(DS.accent).frame(width: 28)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).fontWeight(.semibold)
                Text(text).foregroundStyle(DS.textSecondary).font(.callout)
            }
        }
    }

    private func folderRow(_ path: String, symbol: String, choose: @escaping () -> Void) -> some View {
        HStack {
            Image(systemName: symbol).foregroundStyle(DS.textSecondary)
            Text(path).lineLimit(1).truncationMode(.middle)
            Spacer()
            Button("Choose…", action: choose)
        }
        .padding(DS.Space.s)
        .background(Color.primary.opacity(0.05), in: .rect(cornerRadius: DS.Radius.m))
    }

    private func pickFolder(message: String) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.message = message
        return panel.runModal() == .OK ? panel.url : nil
    }

    private func finish() {
        do {
            try onboarding.finish()
            Task { await onboarding.recordTargetVolume() } // N3
            model.sheet = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}

/// ⌘, preferences.
struct SettingsView: View {
    @Bindable var settings: AppSettings

    var body: some View {
        Form {
            Picker("Appearance", selection: $settings.appearance) {
                ForEach(AppearancePreference.allCases, id: \.self) { Text($0.rawValue) }
            }
            .pickerStyle(.segmented)
            Picker("Open in", selection: $settings.startView) { // [library]
                ForEach(StartView.allCases, id: \.self) { Text($0.title) }
            }
            .help("At launch: the last session, or the Library of every session's photos (no session is opened until you need one)")
            LabeledContent("Sessions folder", value: settings.sessionsFolder.path)
            LabeledContent("Archive, Amazon Photos, Immich, Google Photos", value: "Plugins tab")
            Toggle("Open the import sheet when a card is inserted", isOn: $settings.autoOpenImport)
            TextField("Artist", text: $settings.artist, prompt: Text("Your name"))
                .help("Written into exported files (EXIF Artist)")
            TextField("Copyright", text: $settings.copyright, prompt: Text("© 2026 Your Name"))
                .help("Written into exported files (EXIF Copyright)")
            TextField("New session name", text: $settings.sessionNameTemplate)
                .help("Tokens: {date} {text}")
            Picker("Move to Selects takes picked photos and", selection: $settings.selectsMinRating) {
                ForEach(1...5, id: \.self) { n in
                    Text(n == 5 ? "rated ★★★★★" : "rated \(String(repeating: "★", count: n)) and up").tag(n)
                }
            }
            .help("Photo ▸ Move Picked and Rated to Selects…")
            Toggle("Load map tiles from Apple Maps (Map mode)",
                   isOn: Binding(get: { settings.mapTiles == true }, set: { settings.mapTiles = $0 }))
                .help(MapModel.tilesNotice)
            Button("Show Welcome Again") { settings.onboardingComplete = false }
        }
        .formStyle(.grouped)
        .frame(width: 520)
        .padding(DS.Space.m)
    }
}
