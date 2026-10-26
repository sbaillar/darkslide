import LuxtableCore
import LuxtableUI
import SwiftUI

/// Window toolbar: session, import, view modes, copy/apply, undo/redo, auto, AI, archive.
/// Every item has a text label (or a tooltip where space is tight) and uses
/// `ToolbarPillStyle`, whose disabled state is visibly different on dark chrome.
struct MainToolbar: ToolbarContent {
    @Bindable var model: AppModel

    /// Room for titles: below it the pills go icon-only (their tooltips and accessibility
    /// labels stay) so Copy/Apply and Undo keep a place instead of sliding into overflow.
    private var wide: Bool { ToolbarLayout.showsTitles(viewerWidth: max(model.viewer.state.viewSize.width, model.viewerAreaWidth)) }

    var body: some ToolbarContent {
        ToolbarItem(placement: .navigation) {
            SessionPicker(model: model)
        }
        ToolbarItem(placement: .navigation) {
            Button {
                model.sheet = .importCard(nil)
            } label: {
                Label(ToolbarHelp.importTitle(mode: model.mode, verifies: model.importer is any ImportVerifying),
                      systemImage: "square.and.arrow.down")
            }
            .buttonStyle(ToolbarPillStyle(showsTitle: true,
                                          isSecondary: model.mode == .live && !(model.importer is any ImportVerifying)))
            .help(ToolbarHelp.importButton(mode: model.mode, verifies: model.importer is any ImportVerifying))
            .accessibilityLabel(ToolbarControl.importCard.control(model).label)
            .accessibilityHint(ToolbarControl.importCard.control(model).hint ?? "")
        }
        ToolbarItem(placement: .principal) {
            // The photo tools (Before/After, Compare, Crop, Straighten) float over the viewer
            // (`StageBar`); the toolbar keeps the view switch, titled when the window has room.
            ModeSwitch(mode: $model.browserMode, showsTitles: wide)
                .help(ToolbarHelp.viewerGrid(model.settings.keyBindings))
        }
        // Luxtable's layout: first of the trailing items, so on small windows Undo (also ⌘Z)
        // and the AI, voice and archive pills go to the overflow menu before Copy/Apply. The
        // Capture One layout puts them at the far right; Lightroom Classic's at the panel foot.
        if model.settings.layoutPreset.copyApply == .toolbar {
            ToolbarItem {
                CopyApplyButtons(model: model, showsTitles: wide, names: model.settings.names)
            }
            .sharedBackgroundVisibility(.hidden)
        }
        ToolbarItem {
            HStack(spacing: 2) {
                Button { model.undo() } label: { Label("Undo", systemImage: "arrow.uturn.backward") }
                    .buttonStyle(ToolbarPillStyle())
                    .disabled(!model.history.canUndo)
                    .help(model.history.undoLabel.map { "Undo \($0) (⌘Z)" } ?? "Nothing to undo (⌘Z)")
                    .accessibilityLabel(ToolbarControl.undo.control(model).label)
                Button { model.redo() } label: { Label("Redo", systemImage: "arrow.uturn.forward") }
                    .buttonStyle(ToolbarPillStyle())
                    .disabled(!model.history.canRedo)
                    .help(model.history.redoLabel.map { "Redo \($0) (⇧⌘Z)" } ?? "Nothing to redo (⇧⌘Z)")
                    .accessibilityLabel(ToolbarControl.redo.control(model).label)
            }
        }
        .sharedBackgroundVisibility(.hidden)
        ToolbarItem {
            HStack(spacing: 2) {
                Button {
                    Task { await model.autoAdjust() }
                } label: {
                    Label("Auto", systemImage: "wand.and.stars")
                }
                .buttonStyle(ToolbarPillStyle(showsTitle: wide))
                .disabled(model.focusedID == nil)
                .help(ToolbarHelp.autoAdjust())
                .accessibilityLabel(ToolbarControl.auto.control(model).label)
                OptimizeButton(model: model, showsTitle: wide)
                Button { // [nl9] ⌘K "Edit with words": styled as the field it opens
                    CommandCatalog.perform(.askAI, model: model)
                } label: {
                    Label(wide ? "Edit, command or search" : "Ask", systemImage: "text.bubble")
                }
                .buttonStyle(CommandFieldStyle(showsTitle: wide))
                .disabled(!CommandCatalog.isEnabled(.askAI, model: model))
                .help(ToolbarControl.ask.control(model).hint ?? "")
                .accessibilityLabel(ToolbarControl.ask.control(model).label)
                AICullButton(model: model, showsTitle: wide)
                if let upscaler = model.upscaler {
                    UpscaleProgressView(service: upscaler)
                }
            }
        }
        .sharedBackgroundVisibility(.hidden)
        ToolbarItem {
            VoiceButton(voice: model.voice)
        }
        .sharedBackgroundVisibility(.hidden)
        ToolbarItem {
            RemoteBadge(remote: model.remote) // [remote]
        }
        .sharedBackgroundVisibility(.hidden)
        if model.features.isOn(.archive) { // Settings ▸ Plugins
            ToolbarItem {
                ArchiveBadge(model: model, showsTitle: wide)
            }
            .sharedBackgroundVisibility(.hidden)
        }
        if model.settings.layoutPreset.copyApply == .toolbarEnd { // Capture One's place: the far right
            ToolbarItem {
                CopyApplyButtons(model: model, showsTitles: wide, names: model.settings.names)
            }
            .sharedBackgroundVisibility(.hidden)
        }
    }
}

struct SessionPicker: View {
    @Bindable var model: AppModel
    @State private var open = false

    var body: some View {
        Button {
            open.toggle()
        } label: {
            HStack(spacing: DS.Space.s) {
                Image(systemName: "rectangle.stack").foregroundStyle(DS.accent)
                Text(model.currentSession?.name ?? "No Session")
                    .font(.system(size: 13, weight: .semibold))
                    .lineLimit(1)
                Image(systemName: "chevron.down").font(.system(size: 9, weight: .bold)).foregroundStyle(DS.textSecondary)
            }
            .padding(.horizontal, DS.Space.s)
            .fixedSize()
        }
        .buttonStyle(.plain)
        .help("Switch session")
        .accessibilityLabel(ToolbarControl.session.control(model).label)
        .popover(isPresented: $open, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 2) {
                Text("RECENT SESSIONS").font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
                    .padding(.horizontal, DS.Space.s).padding(.bottom, DS.Space.xs)
                ForEach(model.recentEntries) { e in
                    Button {
                        open = false
                        LocatePanel.openRecent(e.session, model: model)
                    } label: {
                        HStack {
                            Image(systemName: e.isCurrent ? "checkmark" : "rectangle.stack")
                                .frame(width: 16)
                                .foregroundStyle(e.isCurrent ? DS.accent : DS.textSecondary)
                            Text(e.name)
                            Spacer()
                            Text(e.reason ?? e.session.photoCount.formatted()).foregroundStyle(DS.textSecondary).font(DS.Font.value)
                        }
                        .padding(.horizontal, DS.Space.s).padding(.vertical, 5)
                        .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                    .disabled(!model.canOpenRecent)
                    .opacity(e.session.unavailable == nil ? 1 : 0.5)
                    .help([LocatePanel.help(e.session), e.openedText()].compactMap { $0 }.joined(separator: "\n"))
                }
                Divider().padding(.vertical, DS.Space.xs)
                SessionMenuItem(control: .newSession, model: model, symbol: "plus.rectangle.on.rectangle",
                                title: "New Session…", shortcut: "⇧⌘N") {
                    open = false
                    model.beginNewSession()
                }
                .disabled(!CommandCatalog.isEnabled(.newSession, model: model))
                Button("New Session from Card…") {
                    open = false
                    model.sheet = .importCard(nil)
                }
                .buttonStyle(.plain)
                .padding(.horizontal, DS.Space.s)
                Divider().padding(.vertical, DS.Space.xs)
                SessionMenuItem(control: .closeSession, model: model, symbol: "xmark.rectangle",
                                title: "Close Session", shortcut: "⌥⌘W") {
                    open = false
                    Task { await model.closeSession() }
                }
                .disabled(!CommandCatalog.isEnabled(.closeSession, model: model))
            }
            .font(.system(size: 12))
            .padding(DS.Space.s)
            .frame(width: 320)
        }
    }
}

/// A command row in the session menu: icon, title, shortcut; dimmed when disabled
/// (the tooltip says why). Text uses the DS text tokens (≥ 4.5:1).
struct SessionMenuItem: View {
    let control: ToolbarControl
    @Bindable var model: AppModel
    let symbol: String
    let title: String
    let shortcut: String
    let action: () -> Void
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        let c = control.control(model)
        Button(action: action) {
            HStack {
                Image(systemName: symbol).frame(width: 16).foregroundStyle(DS.textSecondary)
                Text(title).foregroundStyle(DS.text)
                Spacer()
                Text(shortcut).font(DS.Font.value).foregroundStyle(DS.textSecondary)
            }
            .opacity(isEnabled ? 1 : 0.45)
            .padding(.horizontal, DS.Space.s).padding(.vertical, 5)
            .contentShape(.rect)
        }
        .buttonStyle(.plain)
        .help(c.hint ?? "")
        .accessibilityLabel(c.label)
        .accessibilityHint(c.hint ?? "")
    }
}

struct AICullButton: View {
    @Bindable var model: AppModel
    var showsTitle = true

    var body: some View {
        if let p = model.cullProgress {
            HStack(spacing: DS.Space.s) {
                ProgressView(value: Double(p.done), total: Double(max(p.total, 1)))
                    .progressViewStyle(.circular)
                    .controlSize(.small)
                Text("\(p.done)/\(p.total)").font(DS.Font.value).foregroundStyle(DS.textSecondary)
            }
            .help("AI Cull is scoring focus, eyes, and composition on this Mac")
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(ToolbarControl.aiCull.control(model).label)
        } else {
            Button {
                Task { await model.runAICull() }
            } label: {
                Label("AI Cull", systemImage: "sparkles")
            }
            .buttonStyle(ToolbarPillStyle(showsTitle: showsTitle))
            .disabled(model.visibleIDs.isEmpty)
            .help("Suggest picks and rejects from sharpness, eyes, and aesthetics. On-device; ⌘Z undoes it.")
            .accessibilityLabel(ToolbarControl.aiCull.control(model).label)
            .accessibilityHint(ToolbarControl.aiCull.control(model).hint ?? "")
        }
    }
}

/// AI Upscale in progress: how far, and a Cancel button. Nothing when idle.
struct UpscaleProgressView: View {
    var service: UpscaleService

    var body: some View {
        if let p = service.progress {
            HStack(spacing: DS.Space.s) {
                ProgressView(value: p.fraction).progressViewStyle(.circular).controlSize(.small)
                Text(p.status).font(DS.Font.value).foregroundStyle(DS.textSecondary)
                Button { service.cancel() } label: { Image(systemName: "xmark.circle.fill") }
                    .buttonStyle(.plain)
                    .help("Stop upscaling; photos already done stay")
                    .accessibilityLabel("Cancel upscale")
            }
            .help("AI Upscale is making a new photo twice the size on this Mac")
            .accessibilityElement(children: .contain)
            .accessibilityLabel(p.status)
        }
    }
}

/// Archive state at a glance; click for details. Always looks like a button.
struct ArchiveBadge: View {
    @Bindable var model: AppModel
    var showsTitle = true

    var body: some View {
        let state = model.archiveStatus.state
        Button {
            model.archivePopoverShown.toggle()
        } label: {
            Label(ArchiveDisplay.short(state), systemImage: ArchiveDisplay.symbol(state))
        }
        .buttonStyle(ToolbarPillStyle(showsTitle: showsTitle))
        .foregroundStyle(tint)
        .help(ArchiveDisplay.badgeHelp(model.archiveStatus, settings: model.settings.archiveSnapshot))
        .accessibilityLabel(ToolbarControl.archive.control(model).label)
        .popover(isPresented: $model.archivePopoverShown, arrowEdge: .bottom) {
            ArchivePopover(model: model)
        }
    }

    private var tint: Color {
        switch model.archiveStatus.state {
        case .idle: DS.pick
        case .syncing: DS.accent
        case .behind, .offline, .unavailable, .notArchived: DS.textSecondary
        case .failed, .problems: DS.reject
        }
    }
}

/// Viewer / Grid switch with a clear accent selection (reads well in dark UI).
struct ModeSwitch: View {
    @Binding var mode: BrowserMode
    var showsTitles = true

    var body: some View {
        HStack(spacing: 2) {
            item(.filmstrip, "rectangle.bottomthird.inset.filled", "Viewer")
            item(.grid, "square.grid.3x3.fill", "Grid")
            item(.map, "map.fill", "Map") // [map]
            item(.library, "books.vertical.fill", "Library") // [library]
        }
        .padding(2)
        .background(Color.primary.opacity(0.07), in: .capsule)
    }

    private func item(_ m: BrowserMode, _ symbol: String, _ name: String) -> some View {
        Button {
            mode = m
        } label: {
            Group {
                if showsTitles {
                    Label(name, systemImage: symbol).labelStyle(.titleAndIcon)
                } else {
                    Label(name, systemImage: symbol).labelStyle(.iconOnly)
                }
            }
                .font(.system(size: 12, weight: .medium))
                .padding(.horizontal, showsTitles ? 10 : 8)
                .padding(.vertical, 4)
                .foregroundStyle(mode == m ? Color.black : DS.textSecondary)
                .background(mode == m ? DS.accent : .clear, in: .capsule)
                .contentShape(.capsule)
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(mode == m ? .isSelected : [])
    }
}

/// The ⌘K button drawn as a search field (where people expect to type), with the shortcut.
struct CommandFieldStyle: ButtonStyle {
    var showsTitle = true
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: DS.Space.s) {
            if showsTitle {
                configuration.label.labelStyle(.titleAndIcon)
                Spacer(minLength: DS.Space.m)
                Text("⌘K").font(DS.Font.caption.monospaced())
                    .padding(.horizontal, 4)
                    .overlay(RoundedRectangle(cornerRadius: 3).strokeBorder(Color.primary.opacity(0.2)))
            } else {
                configuration.label.labelStyle(.iconOnly)
            }
        }
        .font(.system(size: 12))
        .foregroundStyle(isEnabled ? DS.textSecondary : DS.textSecondary.opacity(0.5))
        .padding(.horizontal, 10)
        .frame(width: showsTitle ? 240 : nil, height: 26)
        .background(Color.primary.opacity(configuration.isPressed ? 0.14 : 0.07), in: .rect(cornerRadius: 7))
        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(Color.primary.opacity(0.12)))
        .contentShape(.rect)
    }
}

/// Toolbar density.
enum ToolbarLayout {
    /// Titles on the viewer tools when the viewer is at least this wide (a ~1600 pt window).
    static let titleViewerWidth: CGFloat = 1150

    static func showsTitles(viewerWidth: CGFloat) -> Bool { viewerWidth >= titleViewerWidth }
}

/// AI Optimize (⌥⌘A): opens the panel (it floats over the window; this closes it too).
private struct OptimizeButton: View {
    @Bindable var model: AppModel
    var showsTitle = true

    var body: some View {
        let control = ToolbarControl.optimize.control(model)
        Button { CommandCatalog.perform(.aiOptimize, model: model) } label: {
            Label("Optimize", systemImage: "wand.and.sparkles")
        }
        .buttonStyle(ToolbarPillStyle(showsTitle: showsTitle, isOn: model.optimizePanel != nil))
        .disabled(!CommandCatalog.isEnabled(.aiOptimize, model: model))
        .help(ToolbarHelp.aiOptimize())
        .accessibilityLabel(control.label)
        .accessibilityHint(control.hint ?? "")
    }
}
