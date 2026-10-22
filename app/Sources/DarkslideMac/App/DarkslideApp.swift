import AppKit
import DarkslideCore
import DarkslideUI
import SwiftUI

public struct DarkslideApp: App {
    @State private var model: AppModel
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate

    public init() {
        // L9: a snapshot run that never reaches its capture (even when the window never
        // appears) exits naming the step, instead of hanging.
        if CommandLine.arguments.contains("--snapshot") {
            SnapshotWatchdog.start(seconds: Double(WindowSnapshot.argument("--snapshot-timeout") ?? "") ?? 180)
        }
        // Private-defaults runs (snapshot/dev) turn window restoration off for their window,
        // so AppKit saved "no windows" and the next such run opened none (the main window
        // never appeared). They ignore saved window state instead. The registration domain
        // is in memory only: nothing is written to the user's preferences.
        if AppModel.usesPrivateDefaults {
            UserDefaults.standard.register(defaults: ["ApplePersistenceIgnoreState": true])
        }
        // A path on the command line (`--open <folder>`, `--import-card <volume>`) is not a
        // document for AppKit to open: treated as one, the launch became "open files" and
        // SwiftUI never showed the main window. (In memory only, like the line above.)
        UserDefaults.standard.register(defaults: ["NSTreatUnknownArgumentsAsOpen": "NO"])
        let model = AppModel.live()
        model.confirmClose = SessionCloseAlert.ask // Close Session asks while jobs run
        UpscaleService.shared.confirmDownload = UpscaleService.askDownload // its terms, once
        _model = State(initialValue: model)
        NSWindow.allowsAutomaticWindowTabbing = false
    }

    public var body: some Scene {
        Window("Darkslide", id: AppWindows.main) {
            MainWindow(model: model)
                .frame(minWidth: 1100, minHeight: 700)
                .preferredColorScheme(model.settings.appearance.colorScheme)
                .task {
                    appDelegate.model = model
                    // M4: private-defaults runs (snapshot/dev) don't autosave the window
                    // frame into the user's domain.
                    if AppModel.usesPrivateDefaults {
                        for w in NSApp.windows {
                            w.setFrameAutosaveName("")
                        }
                    }
                    let args = CommandLine.arguments
                    if let i = args.firstIndex(of: "--snapshot"), i + 1 < args.count {
                        model.snapshotMode = true
                        model.settings.onboardingComplete = true
                        model.sheet = nil
                        Task { await WindowSnapshot.run(model: model, path: args[i + 1]) }
                    }
                    await model.start()
                    if let i = args.firstIndex(of: "--open"), i + 1 < args.count {
                        await model.openFolder(URL(fileURLWithPath: args[i + 1]))
                    }
                }
        }
        .windowToolbarStyle(.unified(showsTitle: false))
        .defaultSize(width: 1680, height: 1020)
        .commands { AppCommands(model: model) }

        // Tools ▸ Customize Tools… (⇧⌘T): live, so the sidebar shows each change.
        Window("Customize Tools", id: ToolCustomizeView.windowID) {
            ToolCustomizeView(model: model)
                .preferredColorScheme(model.settings.appearance.colorScheme)
        }
        .windowResizability(.contentMinSize)
        .defaultSize(width: 900, height: 580)
        // Never reopened at launch: the app starts with the main window alone.
        .restorationBehavior(.disabled)
        .defaultLaunchBehavior(.suppressed)

        Settings {
            PreferencesView(settings: model.settings, voice: model.voice, remote: model.remote,
                            googlePhotos: model.googlePhotos)
                .preferredColorScheme(model.settings.appearance.colorScheme)
        }
    }
}

extension AppearancePreference {
    var colorScheme: ColorScheme? {
        switch self {
        case .dark: .dark
        case .light: .light
        case .system: nil
        }
    }
}

/// Quit handling (N5): flush pending saves before AppKit terminates; ask when
/// changes would be lost. The decisions live in `TerminationFlow` (tested).
final class AppDelegate: NSObject, NSApplicationDelegate {
    weak var model: AppModel?

    /// One-window app: closing the main window closes the others (Customize Tools,
    /// Settings) and so quits, through `applicationShouldTerminate` as before. A
    /// lone `Window` scene did that by itself; the Customize Tools scene ended that.
    func applicationDidFinishLaunching(_ notification: Notification) {
        // L6: SwiftUI (re)builds the menus; ⌥⌘W stays Close Session's (never Close All).
        NotificationCenter.default.addObserver(forName: NSMenu.didAddItemNotification, object: nil, queue: .main) { note in
            nonisolated(unsafe) let changed = note.object as? NSMenu // read on the main queue only
            MainActor.assumeIsolated {
                guard let menu = NSApp.mainMenu, let changed, MenuKeys.isInside(changed, menu) else { return }
                MenuKeys.reserveSessionKeys(in: menu)
            }
        }
        if let menu = NSApp.mainMenu { MenuKeys.reserveSessionKeys(in: menu) }
        NotificationCenter.default.addObserver(forName: NSWindow.willCloseNotification, object: nil, queue: .main) { note in
            guard let closing = note.object as? NSWindow else { return }
            MainActor.assumeIsolated {
                guard AppWindows.isMain(closing.identifier?.rawValue) else { return }
                for w in NSApp.windows where w !== closing && w.isVisible && w.sheetParent == nil { w.close() }
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let model = MainActor.assumeIsolated({ self.model }) else { return .terminateNow }
        Task { @MainActor in
            let quit = await TerminationFlow.run(decide: { await model.quitDecision(timeout: .seconds(5)) },
                                                 ask: Self.ask)
            if quit {
                await model.stopImportsForQuit() // [imp7] M3: journals saved, resumes next time (bounded)
                await model.stopExportsForQuit() // [rnd9] M4: cancel the batch (bounded)
                await model.shutdownArchive() // cancels a running rsync (bounded)
                model.remote.stop() // the iPad link stops listening and advertising
            }
            sender.reply(toApplicationShouldTerminate: quit)
        }
        return .terminateLater
    }

    @MainActor
    private static func ask(_ spec: TerminationFlow.Alert) -> TerminationFlow.Choice {
        let alert = NSAlert()
        alert.messageText = spec.title
        alert.informativeText = spec.message
        alert.alertStyle = spec.isCritical ? .critical : .warning
        for c in spec.choices { alert.addButton(withTitle: c.title) }
        let index = alert.runModal().rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
        return spec.choices.indices.contains(index) ? spec.choices[index] : .cancel
    }
}

extension SessionCloseAlert {
    /// The app's question: Cancel (⏎) or Close Session.
    @MainActor
    static func ask(_ spec: SessionCloseAlert) async -> Bool {
        let alert = NSAlert()
        alert.messageText = spec.title
        alert.informativeText = spec.message
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: SessionCloseAlert.confirm)
        return alert.runModal() == .alertSecondButtonReturn
    }
}

extension UpscaleService {
    /// The app's question before the AI Upscale model is downloaded: its terms.
    @MainActor
    static func askDownload(_ notice: String) -> Bool {
        let alert = NSAlert()
        alert.messageText = "Download the AI Upscale model?"
        alert.informativeText = notice
        alert.addButton(withTitle: "Download")
        alert.addButton(withTitle: "Cancel")
        return alert.runModal() == .alertFirstButtonReturn
    }
}

/// Darkslide ▸ About Darkslide: the standard panel, its credits the bundled acknowledgements
/// and third-party licences (`Acknowledgements`).
@MainActor
enum AboutPanel {
    static func show() {
        let credits = NSAttributedString(string: Acknowledgements.text(in: .main), attributes: [
            .font: NSFont.monospacedSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular),
            .foregroundColor: NSColor.labelColor,
        ])
        NSApp.orderFrontStandardAboutPanel(options: [.credits: credits])
        NSApp.activate()
    }
}

/// Menu bar. ⌘-shortcuts live here (standard menu routing, so text fields keep
/// their own ⌘Z/⌘A); plain culling keys go through `KeyMonitor` and are shown
/// in the Photo menu. Titles, enablement and actions come from `CommandCatalog`
/// and `MenuActions` (unit-tested); this only lays them out.
struct AppCommands: Commands {
    let model: AppModel
    @State private var layout = ToolLayout.shared
    @Environment(\.openWindow) private var openWindow

    private func shared(_ a: MenuActions.Shared) {
        if let sel = MenuActions.perform(a, model: model, textEditing: TextEditing.isActive) {
            NSApp.sendAction(sel, to: nil, from: nil)
        }
    }

    private func bound(_ c: BindableCommand) -> some View {
        Button(CommandCatalog.title(c, bindings: model.settings.keyBindings)) { model.perform(c.command) }
            .disabled(!CommandCatalog.isEnabled(c, model: model))
    }

    @ViewBuilder
    private func item(_ c: MenuCommand) -> some View {
        let button = Button(CommandCatalog.title(c, model: model)) {
            switch CommandCatalog.perform(c, model: model, layout: layout) {
            case .chooseFolder: openFolderPanel()
            case .openCustomizeTools: openWindow(id: ToolCustomizeView.windowID)
            case .none: break
            }
        }
        .disabled(!CommandCatalog.isEnabled(c, model: model, layout: layout))
        if let s = model.settings.keyPreset.shortcut(c) {
            button.keyboardShortcut(KeyEquivalent(s.key), modifiers: Self.modifiers(s.modifiers))
        } else {
            button
        }
    }

    private static func modifiers(_ m: Set<MenuCommand.Shortcut.Modifier>) -> EventModifiers {
        var out: EventModifiers = []
        if m.contains(.command) { out.insert(.command) }
        if m.contains(.shift) { out.insert(.shift) }
        if m.contains(.option) { out.insert(.option) }
        if m.contains(.control) { out.insert(.control) }
        return out
    }

    private func toggle(_ title: String, _ key: KeyEquivalent, _ path: ReferenceWritableKeyPath<ViewerModel, Bool>) -> some View {
        Toggle(title, isOn: Bindable(model.viewer)[dynamicMember: path]).keyboardShortcut(key, modifiers: [.command, .option])
    }

    var body: some Commands {
        CommandGroup(replacing: .appInfo) {
            Button("About Darkslide") { AboutPanel.show() }
        }
        CommandGroup(replacing: .undoRedo) {
            Button(model.history.undoLabel.map { "Undo \($0)" } ?? "Undo") { shared(.undo) }
                .keyboardShortcut("z")
                .disabled(!MenuActions.isEnabled(.undo, model: model, textEditing: TextEditing.isActive))
            Button(model.history.redoLabel.map { "Redo \($0)" } ?? "Redo") { shared(.redo) }
                .keyboardShortcut("z", modifiers: [.command, .shift])
                .disabled(!MenuActions.isEnabled(.redo, model: model, textEditing: TextEditing.isActive))
        }
        CommandGroup(after: .pasteboard) {
            Button("Select All Photos") { shared(.selectAll) }
                .keyboardShortcut("a")
            Divider()
            ForEach(CommandCatalog.matchMenu) { item($0) }
            item(.find) // [find] ⌘F
        }
        CommandGroup(after: .newItem) {
            item(.newSession).help(CommandCatalog.help(.newSession, model: model) ?? "")
            item(.openFolder)
            openRecentMenu
            item(.importCard)
            item(.closeSession).help(CommandCatalog.help(.closeSession, model: model) ?? "")
            Button("Import Capture One Edits\u{2026}") { model.showCaptureOneImport() }
                .disabled(!model.canImportCaptureOne)
            Button("Revert Capture One Import\u{2026}") { model.requestRevertCaptureOne() }
                .disabled(!model.canRevertCaptureOne)
            Button("Import Capture One Settings\u{2026}") { model.showCaptureOneSettingsImport() }
            Button("Import Lightroom\u{2026}") { model.showLightroomImport() }
                .disabled(!model.canImportLightroom)
            Button("Revert Lightroom Import\u{2026}") { model.requestRevertLightroom() }
                .disabled(!model.canRevertLightroom)
            Divider()
            item(.exportSelected) // [exp6]
        }
        CommandMenu("Photo") {
            ForEach(Array(CommandCatalog.photoMenu.enumerated()), id: \.offset) { _, c in
                if let c { bound(c) } else { Divider() }
            }
            Divider()
            // [cul9] H7: survey, variants, finishing a cull.
            ForEach(Array(CommandCatalog.cullMenu.enumerated()), id: \.offset) { _, c in
                if let c { item(c).help(CommandCatalog.help(c, model: model) ?? "") } else { Divider() }
            }
            Divider()
            Menu("Enhance") {
                ForEach(CommandCatalog.enhanceMenu) { item($0).help(CommandCatalog.help($0, model: model) ?? "") }
            }
            ForEach(CommandCatalog.mergeMenu) { item($0) } // HDR and panorama merges
            Divider()
            Toggle("Auto-Advance After Rating", isOn: Binding(get: { model.settings.autoAdvance },
                                                                set: { model.settings.autoAdvance = $0 }))
        }
        CommandMenu("Adjustments") {
            item(.copyAdjustments)
            item(.applyAdjustments)
            item(.copyFromPrevious)
            Divider()
            item(.autoAdjust)
            item(.aiOptimize)
            item(.askAI) // [nl9] ⌘K
            item(.aiCull)
            item(.aiChanges)
            item(.undoAllAIChanges)
            item(.showAIChanges)
            item(.clearAIHighlights)
            Divider()
            ForEach(CommandCatalog.toolMenu) { bound($0) } // [cmp9] crop, straighten, color picker
            Divider()
            Toggle("Edit All Selected", isOn: Binding(get: { model.editAllSelected }, set: { model.editAllSelected = $0 }))
        }
        CommandMenu("Tools") { toolsMenu }
        CommandMenu("View") {
            ForEach(CommandCatalog.viewMenu) { bound($0) }
            Toggle(CommandCatalog.title(.showSelectionInViewer, model: model), // [multiview]
                   isOn: Binding(get: { model.settings.showSelectionInViewer }, set: { model.settings.showSelectionInViewer = $0 }))
            Menu("Filmstrip") { FilmstripPositionMenu(model: model) }
            Divider()
            item(.zoomIn)
            item(.zoomOut)
            item(.zoomFit)
            item(.zoomActual)
            Divider()
            ForEach(CommandCatalog.smartFolderMenu) { item($0) } // smart folders: ⌘0, ⌘1…⌘5
            Divider()
            toggle("Clipping Warning", "j", \.clippingWarning)
            toggle("Focus Peaking", "p", \.focusPeaking)
            toggle("Loupe", "l", \.loupe)
            toggle("Navigator", "n", \.showNavigator)
            // ⇧S (the key monitor): a menu key equivalent would toggle it twice.
            Toggle("Soft Proof", isOn: Binding(get: { model.softProof.enabled }, set: { model.softProof.setEnabled($0) }))
            Divider()
            item(.rotateLeft)
            item(.rotateRight)
            Divider()
            item(.moveTabUp)
            item(.moveTabDown)
            item(.showHistory)
            Toggle("Tools on Left", isOn: Binding(get: { model.settings.toolSide == .left },
                                                  set: { model.settings.putTools(on: $0 ? .left : .right) }))
            Divider()
            item(.fullScreen)
            Divider()
            voiceToggle
            item(.voiceHelp)
            item(.cheatSheet)
        }
    }

    /// View ▸ Voice Commands (⌃⌘V), checked while on.
    private var voiceToggle: some View {
        let s = MenuCommand.voiceCommands.shortcut!
        return Toggle(CommandCatalog.title(.voiceCommands, model: model),
                      isOn: Binding(get: { model.voice.isOn }, set: { _ in CommandCatalog.perform(.voiceCommands, model: model) }))
            .keyboardShortcut(KeyEquivalent(s.key), modifiers: Self.modifiers(s.modifiers))
    }

    /// Every tool, grouped by its default tab: choosing one shows it (switching
    /// tab, expanding, scrolling; added back to its tab if it was removed). The
    /// checkmark says whether it is in the current workspace.
    @ViewBuilder
    private var toolsMenu: some View {
        item(.customizeTools)
        Menu("Workspaces") {
            ForEach(layout.saved, id: \.name) { w in
                Button(w.name) { ToolWorkspaceCommands.loadWorkspace(named: w.name, model: model, layout: layout) }
            }
            if !layout.saved.isEmpty { Divider() }
            Button("Save Workspace…") {
                layout.customizeFocus = .workspaceName
                openWindow(id: ToolCustomizeView.windowID)
            }
            Menu("Delete Workspace") {
                ForEach(layout.saved, id: \.name) { w in
                    Button(w.name) { layout.deleteWorkspace(named: w.name) }
                }
            }
            .disabled(layout.saved.isEmpty)
            Divider()
            Button("Reset to Default") { ToolWorkspaceCommands.resetToDefault(model: model, layout: layout) }
            Button("Undo Workspace Change") { ToolWorkspaceCommands.undoWorkspaceChange(model: model, layout: layout) }
                .disabled(layout.undoWorkspace == nil)
        }
        Toggle(CommandCatalog.title(.toggleAllToolsTab, model: model),
               isOn: Binding(get: { layout.workspace.contains(.allTools) },
                             set: { _ in CommandCatalog.perform(.toggleAllToolsTab, model: model, layout: layout) }))
            .disabled(!CommandCatalog.isEnabled(.toggleAllToolsTab, model: model, layout: layout))
        Divider()
        ForEach(ToolWorkspaceCommands.menuSections, id: \.tab) { section in
            Menu(section.tab.title) {
                ForEach(section.panels) { p in
                    Toggle(p.title, isOn: Binding(get: { layout.workspace.containsTool(p) },
                                                  set: { _ in ToolWorkspaceCommands.reveal(p, model: model, layout: layout) }))
                }
            }
        }
    }

    private func openFolderPanel() { OpenFolderPanel.run(model: model) }

    /// File ▸ Open Recent: the recents, newest first; one whose drive isn't connected is
    /// listed greyed out with why. Clear Menu forgets the list, never a folder.
    private var openRecentMenu: some View {
        Menu("Open Recent") {
            ForEach(model.recentEntries) { e in
                Button(e.menuTitle) { LocatePanel.openRecent(e.session, model: model) }
                    .disabled(!model.canOpenRecent)
                    .help(LocatePanel.help(e.session))
            }
            Divider()
            Button("Clear Menu") { Task { await model.clearRecents() } } // the list only, never a folder
                .disabled(!model.canClearRecents)
        }
    }
}

/// Whether a text field currently has keyboard focus (field editor is first responder).
enum TextEditing {
    @MainActor static var isActive: Bool {
        NSApp.keyWindow?.firstResponder is NSTextView
    }
}
