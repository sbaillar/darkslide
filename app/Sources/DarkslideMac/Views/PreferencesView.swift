import AppKit
import DarkslideCore
import DarkslideUI
import SwiftUI

/// ⌘, window: General, Familiar, Shortcuts, Archive, Google Photos, AI, Voice, Devices, Plugins.
struct PreferencesView: View {
    @Bindable var settings: AppSettings
    let voice: VoiceController
    let remote: RemoteLinkModel
    var googlePhotos: (any GooglePhotosService)?

    var body: some View {
        TabView {
            SettingsView(settings: settings)
                .tabItem { Label("General", systemImage: "gearshape") }
            FamiliarSettingsView(settings: settings)
                .tabItem { Label("Familiar", systemImage: "rectangle.split.3x1") }
            ShortcutsSettingsView(settings: settings)
                .tabItem { Label("Shortcuts", systemImage: "keyboard") }
            ArchiveSettingsView(model: ArchiveSettingsModel(settings: settings))
                .tabItem { Label("Archive", systemImage: "externaldrive.badge.icloud") }
            if let googlePhotos {
                GooglePhotosSettingsView(settings: GooglePhotosSettings(defaults: settings.userDefaults), service: googlePhotos)
                    .tabItem { Label("Google Photos", systemImage: "photo.on.rectangle.angled") }
            }
            AISettingsView(model: AISettingsModel(jev: JevSettings(defaults: settings.userDefaults))) // [nl9]
                .tabItem { Label("AI", systemImage: "sparkles") }
            VoiceSettingsView(settings: settings, voice: voice)
                .tabItem { Label("Voice", systemImage: "mic") }
            DevicesSettingsView(model: remote) // [remote]
                .tabItem { Label("Devices", systemImage: "ipad.landscape") }
            PluginsSettingsView(store: PluginStore.shared)
                .tabItem { Label("Plugins", systemImage: "puzzlepiece.extension") }
        }
        .frame(width: 560)
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
