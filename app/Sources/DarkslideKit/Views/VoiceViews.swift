import AppKit
import SwiftUI

/// Toolbar mic: voice commands on/off (⌃⌘V), and the listening indicator.
struct VoiceButton: View {
    @Bindable var voice: VoiceController

    var body: some View {
        Button {
            voice.toggle()
        } label: {
            Label("Voice", systemImage: symbol)
                .symbolEffect(.pulse, isActive: voice.status == .listening)
        }
        .buttonStyle(ToolbarPillStyle(isOn: voice.isOn))
        .contextMenu {
            Button(voice.isOn ? "Turn Voice Commands Off" : "Turn Voice Commands On") { voice.toggle() }
            Button("What Can I Say?") { voice.helpShown = true }
        }
        .help(VoiceText.help(voice.status))
        .accessibilityLabel(VoiceText.accessibilityLabel(voice.status))
    }

    private var symbol: String {
        switch voice.status {
        case .off: "mic.slash"
        case .listening: "mic.fill"
        case .starting, .paused: "mic"
        }
    }
}

/// The echo of each recognized command ("★★★★★", "Next"), and why voice is paused.
struct VoiceHUD: View {
    @Bindable var voice: VoiceController

    var body: some View {
        VStack(spacing: DS.Space.xs) {
            if let echo = voice.echo {
                Label(echo.text, systemImage: "mic.fill")
                    .font(.title3.weight(.semibold))
                    .padding(.horizontal, DS.Space.m)
                    .padding(.vertical, DS.Space.s)
                    .glassEffect(.regular, in: .capsule)
                    .id(echo.id)
                    .transition(.opacity.combined(with: .scale(scale: 0.95)))
                    .accessibilityLabel("Voice command: \(echo.text)")
            } else if let note = VoiceText.pauseNote(voice.status) {
                Label(note, systemImage: "mic.slash")
                    .font(.callout)
                    .foregroundStyle(DS.textSecondary)
                    .padding(.horizontal, DS.Space.m)
                    .padding(.vertical, DS.Space.xs)
                    .glassEffect(.regular, in: .capsule)
            }
        }
        .padding(.top, DS.Space.l)
        .allowsHitTesting(false)
        .animation(.snappy(duration: 0.15), value: voice.echo)
    }
}

/// Keeps voice commands paused while the app is inactive or a text field has the
/// keyboard, and feeds the push-to-talk key.
struct VoiceMonitor: NSViewRepresentable {
    let voice: VoiceController

    func makeNSView(context: Context) -> MonitorView {
        let v = MonitorView()
        v.voice = voice
        return v
    }

    func updateNSView(_ view: MonitorView, context: Context) { view.voice = voice }

    static func dismantleNSView(_ view: MonitorView, coordinator: ()) { view.removeObservers() }

    final class MonitorView: NSView {
        weak var voice: VoiceController?
        private var monitor: Any?
        private var observers: [NSObjectProtocol] = []
        private var responder: NSKeyValueObservation?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            removeObservers()
            guard let window else { return }
            voice?.setAppActive(NSApp.isActive)
            let center = NotificationCenter.default
            for (name, active) in [(NSApplication.didBecomeActiveNotification, true),
                                   (NSApplication.didResignActiveNotification, false)] {
                observers.append(center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.voice?.setAppActive(active) }
                })
            }
            responder = window.observe(\.firstResponder, options: [.initial, .new]) { [weak self] _, _ in
                MainActor.assumeIsolated { self?.refreshTyping() }
            }
            monitor = NSEvent.addLocalMonitorForEvents(matching: [.flagsChanged, .keyDown, .keyUp, .leftMouseUp]) { [weak self] e in
                let consumed = MainActor.assumeIsolated { self?.handle(e) ?? false }
                return consumed ? nil : e
            }
        }

        private func refreshTyping() {
            guard let window else { return }
            voice?.setTyping(FocusedResponder.classify(window.firstResponder) == .text)
        }

        private func handle(_ e: NSEvent) -> Bool {
            refreshTyping()
            guard let voice, e.window === window else { return false }
            switch e.type {
            case .flagsChanged where voice.pushToTalkKey.isModifier:
                // The right ⌥ alone (device flag), so the left one never starts listening.
                return voice.handlePushToTalk(code: e.keyCode, pressed: e.modifierFlags.rawValue & Self.rightOptionFlag != 0)
            case .keyDown where !voice.pushToTalkKey.isModifier && e.modifierFlags.isDisjoint(with: [.command, .control, .option]):
                return voice.handlePushToTalk(code: e.keyCode, pressed: true)
            case .keyUp where !voice.pushToTalkKey.isModifier:
                return voice.handlePushToTalk(code: e.keyCode, pressed: false)
            default:
                return false
            }
        }

        /// `NX_DEVICERALTKEYMASK`: the right ⌥ key is down.
        private static let rightOptionFlag: UInt = 0x40

        func removeObservers() {
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
            observers.forEach(NotificationCenter.default.removeObserver)
            observers = []
            responder = nil
        }
    }
}

/// Denied permission or no on-device recognizer: says so, with System Settings when it helps.
struct VoiceProblemAlert: ViewModifier {
    @Bindable var voice: VoiceController

    func body(content: Content) -> some View {
        content.alert(voice.problem?.title ?? "", isPresented: Binding(get: { voice.problem != nil },
                                                                     set: { if !$0 { voice.dismissProblem() } }),
                      presenting: voice.problem) { problem in
            if let url = problem.settingsURL {
                Button("Open System Settings") { NSWorkspace.shared.open(url) }
            }
            Button("OK", role: .cancel) {}
        } message: { problem in
            Text(problem.message)
        }
    }
}

/// The Voice Commands sheet: every phrase, by category (`VoiceHelp`, generated from
/// the parser's table). Esc, a click outside or “close help” closes it.
struct VoiceHelpOverlay: View {
    let sections: [VoiceHelp.Section]
    let dismiss: () -> Void

    var body: some View {
        ZStack {
            Color.black.opacity(0.35).ignoresSafeArea().onTapGesture(perform: dismiss)
            VStack(alignment: .leading, spacing: DS.Space.m) {
                HStack {
                    Image(systemName: "mic").font(.title2).foregroundStyle(DS.accent)
                    Text("Voice Commands").font(DS.Font.title)
                    Spacer()
                    Button(action: dismiss) { Image(systemName: "xmark.circle.fill").font(.title2) }
                        .buttonStyle(.plain).foregroundStyle(DS.textSecondary)
                        .help("Close (Esc)")
                        .accessibilityLabel("Close voice commands")
                }
                Text(VoiceHelp.intro).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                ScrollView {
                    LazyVGrid(columns: [GridItem(.flexible(), alignment: .topLeading), GridItem(.flexible(), alignment: .topLeading)],
                              alignment: .leading, spacing: DS.Space.l) {
                        ForEach(sections) { section($0) }
                    }
                }
            }
            .padding(DS.Space.l)
            .frame(width: 760, height: 560)
            .glassEffect(.regular, in: .rect(cornerRadius: DS.Radius.l * 1.5))
            .shadow(color: .black.opacity(0.3), radius: 30, y: 10)
        }
    }

    private func section(_ s: VoiceHelp.Section) -> some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Text(s.title.uppercased()).font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
            ForEach(s.rows) { row in
                VStack(alignment: .leading, spacing: 2) {
                    Text(row.summary).font(.system(size: 12, weight: .medium))
                    Text(row.phrases.joined(separator: "  ")).font(.system(size: 12)).foregroundStyle(DS.accent)
                    if let note = row.note {
                        Text(note).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        }
    }
}

/// Preferences ▸ Voice.
struct VoiceSettingsView: View {
    @Bindable var settings: AppSettings
    @Bindable var voice: VoiceController
    @State private var newPhrase = ""
    @State private var newCommand = "key-pick"
    @State private var refusal: String?

    var body: some View {
        Form {
            Toggle("Voice commands (⌃⌘V)", isOn: Binding(get: { voice.isOn }, set: { _ in voice.toggle() }))
            Toggle("Push to talk: listen only while a key is held", isOn: Binding(
                get: { settings.voicePushToTalk },
                set: { settings.voicePushToTalk = $0; voice.pushToTalk = $0 }))
            Picker("Push-to-talk key", selection: Binding(
                get: { settings.voicePushToTalkKey },
                set: { settings.voicePushToTalkKey = $0; voice.pushToTalkKey = $0 })) {
                ForEach(PushToTalkKey.allCases) { Text($0.title).tag($0) }
            }
            .disabled(!settings.voicePushToTalk)
            Toggle("Auto-advance after a rating or flag (keys and voice)", isOn: $settings.autoAdvance)
            Text(VoiceText.languageNote)
                .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                .fixedSize(horizontal: false, vertical: true)
            Button("What Can I Say?") { voice.helpShown = true }
            Section("Your phrases") {
                ForEach(VoicePhraseBook(settings.voicePhrases).rows, id: \.phrase) { row in
                    HStack {
                        Text("“\(row.phrase)”")
                        Text("→ \(row.command.summary)").foregroundStyle(DS.textSecondary)
                        Spacer()
                        Button("Remove") { settings.removeVoicePhrase(row.phrase, voice: voice) }
                    }
                }
                HStack {
                    TextField("Phrase, e.g. “keeper”", text: $newPhrase)
                    Picker("Means", selection: $newCommand) {
                        ForEach(VoicePhraseBook.targets) { g in
                            Section(g.title) { ForEach(g.commands) { Text($0.summary).tag($0.id) } }
                        }
                    }
                    Button("Add") {
                        refusal = settings.addVoicePhrase(newPhrase, for: newCommand, voice: voice)
                        if refusal == nil { newPhrase = "" }
                    }
                    .disabled(newPhrase.trimmingCharacters(in: .whitespaces).isEmpty)
                }
                if let refusal {
                    Text(refusal).font(DS.Font.label).foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        .formStyle(.grouped)
        .padding(DS.Space.m)
    }
}
