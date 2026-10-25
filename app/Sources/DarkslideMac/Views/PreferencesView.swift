import AppKit
import DarkslideCore
import DarkslideUI
import SwiftUI

/// A Settings pane, in sidebar order.
enum SettingsPane: String, CaseIterable, Identifiable {
    case general, familiar, shortcuts, raw, archive, amazonPhotos, googlePhotos, immich, ai, voice, devices, plugins, updates

    var id: Self { self }

    var title: String {
        switch self {
        case .general: "General"
        case .familiar: "Familiar"
        case .shortcuts: "Shortcuts"
        case .raw: "RAW"
        case .archive: "Archive"
        case .amazonPhotos: "Amazon Photos"
        case .googlePhotos: "Google Photos"
        case .immich: "Immich"
        case .ai: "AI"
        case .voice: "Voice"
        case .devices: "Devices"
        case .plugins: "Plugins"
        case .updates: "Updates"
        }
    }

    var symbol: String {
        switch self {
        case .general: "gearshape"
        case .familiar: "rectangle.split.3x1"
        case .shortcuts: "keyboard"
        case .raw: "camera.aperture"
        case .archive: "externaldrive.badge.icloud"
        case .amazonPhotos: "icloud.and.arrow.up"
        case .googlePhotos: "photo.on.rectangle.angled"
        case .immich: "photo.stack"
        case .ai: "sparkles"
        case .voice: "mic"
        case .devices: "ipad.landscape"
        case .plugins: "puzzlepiece.extension"
        case .updates: "arrow.down.circle"
        }
    }

    /// The panes shown: an integration that is off (Settings ▸ Plugins) or unavailable
    /// has none; its settings are kept.
    static func visible(isOn: (NativeFeature) -> Bool, googlePhotos: Bool, immich: Bool) -> [SettingsPane] {
        allCases.filter {
            switch $0 {
            case .archive: isOn(.archive)
            case .amazonPhotos: isOn(.amazonPhotos)
            case .googlePhotos: googlePhotos && isOn(.googlePhotos)
            case .immich: immich && isOn(.immich)
            default: true
            }
        }
    }
}

/// ⌘, window: a sidebar of panes (General, Familiar, Shortcuts, RAW, the integrations
/// that are on, AI, Voice, Devices, Plugins, Updates). A sidebar rather than toolbar
/// tabs: with the integrations on there are more panes than a toolbar fits, and macOS
/// greys out the tabs it moves into the overflow menu.
struct PreferencesView: View {
    @Bindable var settings: AppSettings
    /// Settings ▸ Plugins: an integration that is off has no pane (its settings are kept).
    let features: FeatureGates
    let voice: VoiceController
    let remote: RemoteLinkModel
    var googlePhotos: (any GooglePhotosService)?
    /// Settings ▸ Immich (live only).
    var immich: ImmichModel?
    let updater: Updater
    @State private var selection: SettingsPane? = .general

    var body: some View {
        let panes = SettingsPane.visible(isOn: features.isOn, googlePhotos: googlePhotos != nil, immich: immich != nil)
        NavigationSplitView {
            List(panes, selection: $selection) { pane in
                Label(pane.title, systemImage: pane.symbol).tag(pane)
            }
            .navigationSplitViewColumnWidth(170)
            .toolbar(removing: .sidebarToggle)
        } detail: {
            // A pane turned off while shown (Settings ▸ Plugins) falls back to General.
            let pane = selection.flatMap { panes.contains($0) ? $0 : nil } ?? .general
            // Each pane brings its own Form or scroll view.
            detail(pane)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                .navigationTitle(pane.title)
        }
        .frame(width: 760, height: 560)
    }

    @ViewBuilder private func detail(_ pane: SettingsPane) -> some View {
        switch pane {
        case .general: SettingsView(settings: settings)
        case .familiar: FamiliarSettingsView(settings: settings)
        case .shortcuts: ShortcutsSettingsView(settings: settings)
        case .raw: RawSettingsView(helper: RawlerHelper.shared)
        case .archive: ArchiveSettingsView(model: ArchiveSettingsModel(settings: settings))
        case .amazonPhotos: AmazonPhotosSettingsView(model: ArchiveSettingsModel(settings: settings))
        case .googlePhotos:
            if let googlePhotos {
                GooglePhotosSettingsView(settings: GooglePhotosSettings(defaults: settings.userDefaults), service: googlePhotos)
            }
        case .immich:
            if let immich {
                ImmichSettingsView(model: immich, archiveTarget: settings.archiveTarget,
                                   objectStorage: settings.archiveDestination == .object)
            }
        case .ai: AISettingsView(model: AISettingsModel(jev: JevSettings(defaults: settings.userDefaults))) // [nl9]
        case .voice: VoiceSettingsView(settings: settings, voice: voice)
        case .devices: DevicesSettingsView(model: remote) // [remote]
        case .plugins: PluginsSettingsView(store: PluginStore.shared)
        case .updates: UpdateSettingsView(updater: updater)
        }
    }
}

/// Rebind the single-key culling shortcuts (UX9).
struct ShortcutsSettingsView: View {
    @Bindable var settings: AppSettings
    @State private var recording: BindableCommand?
    @State private var note: String?

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Text("\(FamiliarText.keysStatus(settings.keyPreset, settings.keyBindings)) (choose the set in Familiar). Click a key to change it, then press the new key (Esc cancels). A key already in use moves to the new command.")
                .font(DS.Font.label).foregroundStyle(DS.textSecondary)
            List(BindableCommand.allCases) { cmd in
                HStack {
                    Text(cmd.title)
                    Spacer()
                    Button(recording == cmd ? "Press a key…" : ShortcutRecorder.keyTitle(cmd, in: settings.keyBindings)) {
                        recording = recording == cmd ? nil : cmd
                    }
                    .frame(minWidth: 90)
                    .accessibilityLabel("\(cmd.title) shortcut: \(ShortcutRecorder.keyTitle(cmd, in: settings.keyBindings))")
                    .background(KeyRecorder(active: recording == cmd) { key in record(cmd, key) })
                }
            }
            .frame(height: 360)
            HStack {
                if let note { Text(note).font(DS.Font.label).foregroundStyle(DS.textSecondary) }
                Spacer()
                Button("Reset to \(settings.keyPreset.title)") { settings.useKeyPreset(settings.keyPreset) }
            }
        }
        .padding(DS.Space.m)
    }

    private func record(_ cmd: BindableCommand, _ key: KeyInput) {
        let out = ShortcutRecorder.record(key, for: cmd, in: settings.keyBindings)
        if out.bindings != settings.keyBindings { settings.keyBindings = out.bindings }
        note = out.note
        if out.done { recording = nil }
    }
}

/// Captures the next key press while `active`, as the culling monitor will read it
/// (`KeyInput(event:)`: digit row by position, so AZERTY bindings fire).
private struct KeyRecorder: NSViewRepresentable {
    let active: Bool
    let onKey: (KeyInput) -> Void

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        context.coordinator.onKey = onKey
        context.coordinator.set(active)
    }

    static func dismantleNSView(_ view: NSView, coordinator: Coordinator) { coordinator.set(false) }

    func makeCoordinator() -> Coordinator { Coordinator() }

    @MainActor
    final class Coordinator {
        var onKey: ((KeyInput) -> Void)?
        private var monitor: Any?

        func set(_ active: Bool) {
            if active, monitor == nil {
                let deliver: (KeyInput) -> Void = { [weak self] k in self?.onKey?(k) }
                monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { e in
                    // ⌘ combinations keep working (e.g. ⌘W closes the window).
                    if e.modifierFlags.contains(.command) { return e }
                    let key = KeyInput(event: e)
                    MainActor.assumeIsolated { deliver(key) }
                    return nil
                }
            } else if !active, let m = monitor {
                NSEvent.removeMonitor(m)
                monitor = nil
            }
        }
    }
}
