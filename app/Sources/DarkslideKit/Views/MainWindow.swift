import SwiftUI

/// Tool tabs + tool panels on the side the familiar layout picks (right for Darkslide
/// and Lightroom Classic, left for Capture One) or View ▸ Tools on Left sets, viewer in the center, filmstrip below
/// (or a full grid in place of the viewer).
struct MainWindow: View {
    @Bindable var model: AppModel

    private var chromeHidden: Bool { model.panelsHidden || model.viewer.state.fullScreen }

    var body: some View {
        let side = model.settings.toolSide
        HStack(spacing: 0) {
            if !chromeHidden && side == .left {
                ToolSidebar(model: model, side: side)
                    .zIndex(1) // tab name flags draw over the viewer
                Divider()
            }
            VStack(spacing: 0) {
                switch model.browserMode {
                case .grid:
                    BrowserGrid(model: model)
                        .overlay(alignment: .top) { ProblemBanner(model: model, sessionOnly: true) }
                        .overlay(alignment: .bottom) { ToastOverlay(toast: model.toast, solid: model.snapshotMode) { Task { await model.openToastSession() } } }
                case .filmstrip:
                    // [multiview] The separator beside the strip drags it larger, in whole rows
                    // (or columns: at the left or right it sits between the viewer and the tools).
                    GeometryReader { geo in
                        let place = model.filmstrip.position
                        let available = place.isVertical ? geo.size.width : geo.size.height
                        let lines = FilmstripSizing.clamped(model.filmstrip.lines, available: available, at: place)
                        let showStrip = !chromeHidden && !model.filmstripHidden
                        let strip = Group {
                            if lines > 0 {
                                Filmstrip(model: model, lines: lines, position: place)
                                    .frame(width: place.isVertical ? FilmstripSizing.extent(lines: lines, at: place) : nil,
                                           height: place.isVertical ? nil : FilmstripSizing.extent(lines: lines, at: place))
                            }
                        }
                        let separator = FilmstripSeparator(model: model, lines: lines, available: available)
                        // The toast sits over the viewer's bottom edge, clear of the filmstrip's
                        // filter bar and the status bar.
                        let viewer = ViewerArea(model: model)
                            .overlay(alignment: .top) { if !model.viewer.state.fullScreen { StageBar(model: model) } }
                            .overlay(alignment: .bottom) { ToastOverlay(toast: model.toast, solid: model.snapshotMode) { Task { await model.openToastSession() } } }
                        switch place {
                        case .bottom:
                            VStack(spacing: 0) {
                                viewer
                                if showStrip { separator; strip }
                            }
                        case .left:
                            HStack(spacing: 0) {
                                if showStrip { strip; separator }
                                viewer
                            }
                        case .right:
                            HStack(spacing: 0) {
                                viewer
                                if showStrip { separator; strip }
                            }
                        }
                    }
                case .library: // [library] every session's photos
                    LibraryBrowser(model: model, library: model.library)
                        .overlay(alignment: .bottom) { ToastOverlay(toast: model.toast, solid: model.snapshotMode) { Task { await model.openToastSession() } } }
                case .map: // [map] the filmstrip stays: photos are dragged from it onto the map
                    MapBrowser(model: model, map: model.map)
                        .overlay(alignment: .bottom) { ToastOverlay(toast: model.toast, solid: model.snapshotMode) { Task { await model.openToastSession() } } }
                    Divider()
                    Filmstrip(model: model)
                        .frame(height: DS.Size.filmstrip + 40)
                }
                if !chromeHidden {
                    Divider()
                    StatusBar(model: model)
                }
            }
            if !chromeHidden && side == .right {
                Divider()
                ToolSidebar(model: model, side: side)
                    .zIndex(1)
            }
        }
        .background(DS.viewerBackground)
        .toolbar { MainToolbar(model: model) }
        .toolbarBackground(DS.chrome, for: .windowToolbar)
        .overlay { // [cul9] N-up survey (⌥⌘S)
            if model.survey.isActive { SurveyView(model: model).transition(.opacity) }
        }
        .overlay(alignment: .bottom) { // over the survey, which covers the browser
            if model.survey.isActive { ToastOverlay(toast: model.toast, solid: model.snapshotMode) { Task { await model.openToastSession() } } }
        }
        .modifier(CullDialogs(model: model)) // [cul9] H7: Delete Rejected… confirmation
        .modifier(CaptureOneRevertDialog(model: model))
        .modifier(LightroomRevertDialog(model: model))
        .overlay(alignment: .topTrailing) { SessionResultsPanel(model: model) } // [search] all sessions
        .overlay {
            if let p = model.openProgress, OpenProgressText.blocksBrowsing(p) {
                OpenProgressCard(model: model, progress: p)
                    .transition(.opacity)
            }
        }
        .overlay {
            if model.showCheatSheet {
                CheatSheetOverlay(bindings: model.settings.keyBindings) { model.showCheatSheet = false }
                    .transition(.opacity.combined(with: .scale(scale: 0.98)))
            }
        }
        .overlay(alignment: .topTrailing) { // AI Optimize: always on screen while open
            GeometryReader { geo in
                if let panel = model.optimizePanel {
                    OptimizePanelView(panel: panel, maxHeight: geo.size.height - 2 * DS.Space.m) { model.endOptimize() }
                        .padding(DS.Space.m)
                        .frame(maxWidth: .infinity, alignment: .topTrailing)
                        .transition(.opacity.combined(with: .move(edge: .trailing)))
                }
            }
        }
        .overlay(alignment: .topTrailing) { // the AI Changes list (hidden while AI Optimize, in the same place, is open)
            GeometryReader { geo in
                if model.aiChangesShown, model.optimizePanel == nil {
                    AIChangesPanel(model: model, maxHeight: geo.size.height - 2 * DS.Space.m)
                        .padding(DS.Space.m)
                        .frame(maxWidth: .infinity, alignment: .topTrailing)
                        .transition(.opacity.combined(with: .move(edge: .trailing)))
                }
            }
        }
        .animation(.snappy(duration: 0.2), value: model.aiChangesShown)
        .onDisappear { model.endOptimize() } // the window closed
        .animation(.snappy(duration: 0.2), value: model.optimizePanel != nil)
        .overlay(alignment: .top) { // [nl9] ⌘K "Edit with words"
            if model.commandBar.isPresented {
                CommandBarView(model: model, bar: model.commandBar)
                    .padding(.top, 60)
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .animation(.snappy(duration: 0.2), value: model.commandBar.isPresented)
        .overlay(alignment: .top) { // [find] ⌘F across every session
            if model.find.isPresented {
                FindPanel(model: model, find: model.find)
                    .padding(.top, 48)
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .animation(.snappy(duration: 0.2), value: model.find.isPresented)
        .animation(.snappy(duration: 0.2), value: model.showCheatSheet)
        .animation(.snappy(duration: 0.25), value: chromeHidden)
        .animation(.snappy(duration: 0.2), value: model.toast)
        .onChange(of: model.sheet) { old, new in
            if old != nil, new == nil { model.sheetDidClose() }
        }
        .sheet(item: $model.sheet) { sheet in
            switch sheet {
            case .importCard(let volume): ImportSheet(model: model, volume: volume)
            case .onboarding: OnboardingView(model: model)
            case .export: if let e = model.exportModel { ExportSheet(app: model, model: e) } // [exp6]
            case .newSession: NewSessionSheet(model: model)
            case .openSession: OpenSessionSheet(model: model)
            case .captureOneImport(let m): CaptureOneImportSheet(app: model, model: m)
            case .captureOneSettings(let m): C1SettingsSheet(app: model, model: m)
            case .lightroomImport(let m): LightroomImportSheet(app: model, model: m)
            case .merge(let m): MergeSheet(model: m)
            }
        }
        .background(KeyMonitor(model: model))
        .overlay {
            if model.voice.helpShown {
                VoiceHelpOverlay(sections: VoiceHelp.sections(custom: VoicePhraseBook(model.settings.voicePhrases))) {
                    model.voice.helpShown = false
                }
                .transition(.opacity.combined(with: .scale(scale: 0.98)))
            }
        }
        .animation(.snappy(duration: 0.2), value: model.voice.helpShown)
        .overlay(alignment: .top) { VoiceHUD(voice: model.voice) } // voice commands: what was heard
        .background(VoiceMonitor(voice: model.voice))
        .modifier(VoiceProblemAlert(voice: model.voice))
        .modifier(PhotoDropTarget(model: model))
        .modifier(ImportResumePrompt(model: model)) // [imp6]
        .onChange(of: model.viewer.state.fullScreen) { _, full in
            if let w = NSApp.keyWindow, w.styleMask.contains(.fullScreen) != full { w.toggleFullScreen(nil) }
        }
    }
}

/// Bottom status line: counts, filter, zoom, engine.
struct StatusBar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: DS.Space.m) {
            Text(countText)
            if model.filter.isActive {
                Label("Filtered", systemImage: "line.3.horizontal.decrease.circle.fill")
                    .foregroundStyle(DS.accent)
            }
            SaveStatusView(model: model)
            SearchStatusLabel(search: model.search) // [search]
            if let m = model.people.indexingMessage { // [find]
                Label(m, systemImage: "person.crop.rectangle").foregroundStyle(DS.textSecondary).accessibilityLabel(m)
            }
            if let p = model.openProgress, !OpenProgressText.blocksBrowsing(p) {
                Label(OpenProgressText.phase(p), systemImage: "checkmark.shield")
                    .foregroundStyle(DS.textSecondary)
                    .accessibilityLabel(OpenProgressText.phase(p))
            }
            if model.editAllSelected {
                Label("Editing all selected", systemImage: "square.stack.3d.up.fill").foregroundStyle(DS.accent)
            }
            Spacer()
            if let p = model.focusedPhoto {
                Text(p.filename).fontWeight(.medium)
                Text("\(Formatters.shutter(p.meta.shutter))  \(Formatters.aperture(p.meta.aperture))  ISO \(p.meta.iso)  \(Formatters.focal(p.meta.focalLength))")
                    .foregroundStyle(DS.textSecondary)
            }
            Text(model.viewer.zoomLabel)
                .foregroundStyle(DS.textSecondary)
                .frame(minWidth: 32, alignment: .trailing)
            SettingsLink { // one click from "where did Paste go?" to Settings › Familiar
                Text(FamiliarText.statusLine(model.settings.layoutPreset, model.settings.keyPreset))
                    .foregroundStyle(DS.textSecondary)
            }
            .buttonStyle(.plain)
            .help("Familiar layout and keys: change them in Settings › Familiar")
        }
        .font(DS.Font.label)
        .lineLimit(1)
        .padding(.horizontal, DS.Space.m)
        .frame(height: 24)
        .background(DS.chrome)
    }

    private var countText: String {
        let n = model.visibleIDs.count
        let sel = model.selection.count
        let base = "\(n.formatted()) photo\(n == 1 ? "" : "s")"
        return sel > 1 ? "\(base) · \(sel.formatted()) selected" : base
    }
}

/// Transient confirmation at the bottom of the window.
struct ToastOverlay: View {
    let toast: Toast?
    /// `--snapshot`: a solid capsule (window captures can't draw the glass).
    var solid = false
    /// Its "Open" button (a toast with `openSession`).
    var open: () -> Void = {}

    var body: some View {
        if let toast {
            HStack(spacing: DS.Space.s) {
                Label(toast.message, systemImage: toast.symbol)
                if toast.openSession != nil {
                    Button("Open", action: open).buttonStyle(.borderless).foregroundStyle(DS.accent)
                }
            }
                .font(.callout.weight(.medium))
                .foregroundStyle(toast.isError ? DS.reject : .primary)
                .padding(.horizontal, DS.Space.m)
                .padding(.vertical, DS.Space.s)
                .background { if solid { Capsule().fill(DS.panelBackground) } }
                .glassEffect(.regular, in: .capsule)
                .padding(.bottom, DS.Space.l)
                .transition(.move(edge: .bottom).combined(with: .opacity))
                .id(toast.id)
                .allowsHitTesting(toast.openSession != nil)
        }
    }
}

/// Persistent save state (M7): demo/memory-only notice, pending, failed + Retry.
struct SaveStatusView: View {
    @Bindable var model: AppModel

    var body: some View {
        if model.mode == .demo {
            Label("Demo data", systemImage: "theatermasks")
                .foregroundStyle(DS.accent)
                .help("Running with --demo: sample sessions, simulated import and archive. Nothing touches your files.")
        } else if !model.persistsChanges, model.currentSession != nil {
            Label("Not saved to disk", systemImage: "externaldrive.badge.exclamationmark")
                .foregroundStyle(DS.star)
                .help("Ratings and edits live in memory in this mode. They are lost when you quit.")
        }
        switch model.saver.state {
        case .saved: EmptyView()
        case .pending, .saving:
            // L14: memory-only stores "save" instantly; don't flash next to "Not saved to disk".
            if model.persistsChanges {
                Label("Saving…", systemImage: "arrow.triangle.2.circlepath").foregroundStyle(DS.textSecondary)
            }
        case .failed(let message, let pending):
            HStack(spacing: DS.Space.xs) {
                Label("\(pending) unsaved change\(pending == 1 ? "" : "s")", systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(DS.reject)
                    .help(message)
                Button("Retry") { Task { await model.saver.retry() } }
                    .controlSize(.mini)
            }
        case .dropped(let message, let count):
            HStack(spacing: DS.Space.xs) {
                Label("\(count) change\(count == 1 ? "" : "s") not saved", systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(DS.reject)
                    .help(message)
                Button("Dismiss") { model.saver.dismissDropped() }
                    .controlSize(.mini)
            }
        }
    }
}

/// Cancellable first-open progress (R3-M2): shown while scanning/registering;
/// hashing continues in the background with only a status-bar note.
struct OpenProgressCard: View {
    @Bindable var model: AppModel
    let progress: OpenProgress

    var body: some View {
        ZStack {
            Color.black.opacity(0.35).ignoresSafeArea()
            VStack(alignment: .leading, spacing: DS.Space.m) {
                Text(OpenProgressText.title(folder: model.openingFolder)).font(.headline)
                if progress.total > 0 {
                    ProgressView(value: progress.fraction)
                } else {
                    ProgressView().progressViewStyle(.linear)
                }
                Text(OpenProgressText.phase(progress)).font(DS.Font.label).foregroundStyle(DS.textSecondary)
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { model.cancelOpen() }
                        .keyboardShortcut(.cancelAction)
                        .help(OpenProgressText.cancel.hint ?? "")
                        .accessibilityLabel(OpenProgressText.cancel.label)
                }
            }
            .padding(DS.Space.l)
            .frame(width: 380)
            .background(DS.panelBackground, in: .rect(cornerRadius: DS.Radius.l))
            .shadow(radius: 20)
            .accessibilityElement(children: .contain)
            .accessibilityLabel(OpenProgressText.title(folder: model.openingFolder))
        }
    }
}
