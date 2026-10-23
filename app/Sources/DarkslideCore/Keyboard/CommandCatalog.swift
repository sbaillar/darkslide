import Foundation

/// Menu-bar commands (M10): titles, shortcuts, enablement and actions live here,
/// unit-tested; `AppCommands` only lays them out.
public enum MenuCommand: String, CaseIterable, Sendable, Identifiable {
    case openFolder, importCard
    /// File ▸ New Session… / Close Session (also in the toolbar's session menu).
    case newSession, closeSession
    case copyAdjustments, applyAdjustments, copyFromPrevious, autoAdjust, aiCull
    /// ⌥⌘A: AI Optimize (explained, measured proposals; a panel to choose them).
    case aiOptimize
    /// [nl9] ⌘K: the "Edit with words" command bar.
    case askAI
    /// [find] ⌘F: Find across every session ("pics of alex at the beach").
    case find
    /// Adjustments ▸ Show AI-Changed Tools / Clear AI Highlights (the tool rows an AI edit changed).
    case showAIChanges, clearAIHighlights
    /// Adjustments ▸ AI Changes… (⌥⇧⌘A): the list of settings AI changed, and Undo All AI Changes.
    case aiChanges, undoAllAIChanges
    case zoomIn, zoomOut, zoomFit, zoomActual
    case rotateLeft, rotateRight, fullScreen, cheatSheet
    case moveTabUp, moveTabDown
    /// Library ▸ Smart Folders: ⌘0 all photos, ⌘1…⌘5 the exact-rating folders.
    case showAll, showRating1, showRating2, showRating3, showRating4, showRating5
    /// File ▸ Export… (⌘E): the selected photos through the export recipes.
    case exportSelected
    /// [cul9] H7: Photo ▸ finishing a cull, variants, and survey mode.
    case moveRejectedToTrash, movePickedToSelects, restoreSessionTrash, deleteRejected, newVariant, survey
    /// Edit ▸ match the selection to the focused photo (`MatchSet.swift`).
    case matchExposures, matchWhiteBalance
    /// Tools ▸ Customize Tools… (⇧⌘T) and the All Tools tab.
    case customizeTools, toggleAllToolsTab
    /// View ▸ Voice Commands (⌃⌘V): spoken culling commands on/off.
    case voiceCommands
    /// View ▸ What Can I Say?: the Voice Commands sheet (every phrase, by category).
    case voiceHelp
    /// Photo ▸ Enhance ▸ Upscale 2× (AI Upscale: a new photo twice the size); Cancel
    /// Upscale while it runs.
    case upscale
    /// Photo ▸ Merge to HDR… (⇧⌘H) / Merge to Panorama… (⇧⌘M): two or more selected photos.
    case mergeHDR, mergePanorama
    /// [history] View ▸ Show History: the History panel, wherever it lives.
    case showHistory
    /// [multiview] View ▸ Show Selection in Viewer: several selected photos side by side.
    case showSelectionInViewer
    /// [multiview] View ▸ Filmstrip ▸ Bottom / Left / Right (also the strip's context menu).
    case filmstripBottom, filmstripLeft, filmstripRight
    /// [multiview] View ▸ Larger / Smaller Filmstrip Thumbnails (⌃⌘= / ⌃⌘-; the strip's size slider).
    case largerFilmstripThumbnails, smallerFilmstripThumbnails

    public var id: String { rawValue }

    public struct Shortcut: Equatable, Sendable {
        public enum Modifier: Sendable { case command, shift, option, control }
        public var key: Character
        public var modifiers: Set<Modifier>
    }

    public var shortcut: Shortcut? {
        switch self {
        case .openFolder: Shortcut(key: "o", modifiers: [.command])
        case .importCard: Shortcut(key: "i", modifiers: [.command, .shift])
        case .newSession: Shortcut(key: "n", modifiers: [.command, .shift])
        // ⌘W still closes the window.
        case .closeSession: Shortcut(key: "w", modifiers: [.command, .option])
        case .copyAdjustments: Shortcut(key: "c", modifiers: [.command, .shift])
        case .applyAdjustments: Shortcut(key: "v", modifiers: [.command, .shift])
        case .copyFromPrevious: Shortcut(key: "v", modifiers: [.command, .option])
        case .autoAdjust: Shortcut(key: "a", modifiers: [.command, .shift])
        case .aiOptimize: Shortcut(key: "a", modifiers: [.command, .option])
        case .aiCull: Shortcut(key: "k", modifiers: [.command, .shift])
        // AI Optimize's ⌥⌘A, plus ⇧.
        case .aiChanges: Shortcut(key: "a", modifiers: [.command, .option, .shift])
        case .askAI: Shortcut(key: "k", modifiers: [.command])
        case .find: Shortcut(key: "f", modifiers: [.command])
        case .zoomIn: Shortcut(key: "+", modifiers: [.command])
        case .zoomOut: Shortcut(key: "-", modifiers: [.command])
        // ⌘0 shows all photos (smart folders); fit pairs with ⌥⌘1 actual size.
        case .zoomFit: Shortcut(key: "0", modifiers: [.command, .option])
        case .zoomActual: Shortcut(key: "1", modifiers: [.command, .option])
        case .rotateLeft: Shortcut(key: "[", modifiers: [.command])
        case .rotateRight: Shortcut(key: "]", modifiers: [.command])
        case .fullScreen: Shortcut(key: "f", modifiers: [.command, .control])
        case .cheatSheet: Shortcut(key: "/", modifiers: [.command])
        case .moveTabUp: Shortcut(key: "[", modifiers: [.command, .option])
        case .moveTabDown: Shortcut(key: "]", modifiers: [.command, .option])
        case .showAll: Shortcut(key: "0", modifiers: [.command])
        case .exportSelected: Shortcut(key: "e", modifiers: [.command])
        case .showRating1, .showRating2, .showRating3, .showRating4, .showRating5:
            Shortcut(key: rawValue.last!, modifiers: [.command])
        case .newVariant: Shortcut(key: "d", modifiers: [.command])
        case .survey: Shortcut(key: "s", modifiers: [.command, .option])
        // Lightroom's binding; ⌥⌘M is the Window menu's Minimize All.
        case .matchExposures: Shortcut(key: "m", modifiers: [.command, .option, .shift])
        case .customizeTools: Shortcut(key: "t", modifiers: [.command, .shift])
        // ⌥⌘V (copy from previous) and ⇧⌘V (apply) are taken; ⌃⌘ is already the
        // full-screen family (⌃⌘F), and no system-wide shortcut uses ⌃⌘V.
        case .voiceCommands: Shortcut(key: "v", modifiers: [.command, .control])
        // Not Lightroom's ⌃H / ⌃M: those fire inside text fields (⌃H deletes backward there).
        // ⌥⌘H is the system's Hide Others; ⌥⇧⌘M (match exposures) is not ⇧⌘M.
        case .mergeHDR: Shortcut(key: "h", modifiers: [.command, .shift])
        case .mergePanorama: Shortcut(key: "m", modifiers: [.command, .shift])
        // ⌘+ / ⌘- zoom the photo and ⌥⌘= / ⌥⌘- are the system's Accessibility Zoom.
        case .largerFilmstripThumbnails: Shortcut(key: "=", modifiers: [.command, .control])
        case .smallerFilmstripThumbnails: Shortcut(key: "-", modifiers: [.command, .control])
        case .moveRejectedToTrash, .movePickedToSelects, .restoreSessionTrash, .deleteRejected, .matchWhiteBalance,
             .toggleAllToolsTab, .showAIChanges, .clearAIHighlights, .undoAllAIChanges, .upscale, .showHistory,
             .voiceHelp, .showSelectionInViewer, .filmstripBottom, .filmstripLeft, .filmstripRight: nil
        }
    }

    /// Where a `filmstripX` command puts the filmstrip.
    public var filmstripPosition: FilmstripPosition? {
        switch self {
        case .filmstripBottom: .bottom
        case .filmstripLeft: .left
        case .filmstripRight: .right
        default: nil
        }
    }

    /// The exact rating a `showRatingN` command shows.
    public var ratingFolder: Int? {
        rawValue.hasPrefix("showRating") ? rawValue.last?.wholeNumberValue : nil
    }

    /// What the view layer must do itself (panels need AppKit).
    public enum Effect: Equatable, Sendable {
        case none
        case chooseFolder
        case openCustomizeTools
    }
}

@MainActor
public enum CommandCatalog {
    public static func title(_ c: MenuCommand, model: AppModel) -> String {
        switch c {
        case .openFolder: "Open Folder…"
        // H5: only a live build without a verifying importer is "coming soon".
        case .importCard: model.mode == .live && !(model.importer is any ImportVerifying)
            ? "Import from Card (Coming Soon)…" : "Import from Card…"
        case .newSession: CommandCatalogTitles.newSession
        case .closeSession: CommandCatalogTitles.closeSession
        case .copyAdjustments: model.settings.names.copyMenu
        case .applyAdjustments: model.settings.names.applyMenu
        case .copyFromPrevious: "Copy from Previous"
        case .autoAdjust: "Auto Adjust"
        case .aiOptimize: model.optimizePanel != nil ? "Close AI Optimize"
            : model.optimizeTargets.count > 1 ? "AI Optimize \(model.optimizeTargets.count) Photos…" : "AI Optimize…"
        case .aiCull: "AI Cull"
        case .askAI: "Edit with Words…"
        case .find: "Find…"
        case .showAIChanges: AIHighlightStyle.showTitle
        case .clearAIHighlights: AIHighlightStyle.clearTitle
        case .aiChanges: model.aiChangesShown ? "Hide AI Changes" : AIChangeText.menuTitle
        case .undoAllAIChanges: model.undoAllAIChangesTitle
        case .zoomIn: "Zoom In"
        case .zoomOut: "Zoom Out"
        case .zoomFit: "Zoom to Fit"
        case .zoomActual: "Actual Size (100%)"
        case .rotateLeft: "Rotate Left"
        case .rotateRight: "Rotate Right"
        case .fullScreen: model.viewer.state.fullScreen ? "Exit Full Screen" : "Enter Full Screen"
        case .cheatSheet: "Keyboard Shortcuts"
        case .moveTabUp: "Move Tool Tab Left"
        case .moveTabDown: "Move Tool Tab Right"
        case .showAll: "Show All Photos"
        case .exportSelected: model.selectedIDs.count > 1 ? "Export \(model.selectedIDs.count) Photos…" : "Export…"
        case .showRating1, .showRating2, .showRating3, .showRating4, .showRating5:
            "Show \(c.ratingFolder!) \(c.ratingFolder == 1 ? "Star" : "Stars") Only"
        case .moveRejectedToTrash: "Move Rejected to Trash"
        case .movePickedToSelects: "Move Picked and Rated to Selects…"
        case .restoreSessionTrash: "Restore from Session Trash"
        case .deleteRejected: "Delete Rejected…"
        case .newVariant: model.settings.names.newVariant
        case .survey: model.survey.isActive ? "Leave Survey" : "Survey Selected"
        case .matchExposures: "Match Total Exposures"
        case .matchWhiteBalance: "Match White Balance to Focused Photo"
        case .customizeTools: "Customize Tools…"
        case .toggleAllToolsTab: "Show All Tools Tab"
        case .showHistory: "Show History"
        case .showSelectionInViewer: "Show Selection in Viewer"
        case .filmstripBottom, .filmstripLeft, .filmstripRight: c.filmstripPosition!.title
        case .largerFilmstripThumbnails: "Larger Filmstrip Thumbnails"
        case .smallerFilmstripThumbnails: "Smaller Filmstrip Thumbnails"
        case .voiceCommands: "Voice Commands"
        case .voiceHelp: "What Can I Say?"
        case .upscale: model.upscaler?.title(in: model) ?? "Upscale 2×"
        case .mergeHDR: "Merge to HDR…"
        case .mergePanorama: "Merge to Panorama…"
        }
    }

    /// [cul9] H7: Photo ▸ finishing a cull (after the plain-key items).
    public static let cullMenu: [MenuCommand?] = [
        .survey, .newVariant, nil, .moveRejectedToTrash, .movePickedToSelects, .restoreSessionTrash, nil, .deleteRejected,
    ]

    /// Photo ▸ merging the selection into a new photo.
    public static let mergeMenu: [MenuCommand] = [.mergeHDR, .mergePanorama]

    /// The browser's context menu (grid and filmstrip): the cull-finishing commands
    /// are not only in the Photo menu (final-journeys2 discoverability).
    public static let cullContextMenu: [MenuCommand?] = [
        .survey, nil, .moveRejectedToTrash, .movePickedToSelects, .restoreSessionTrash, nil, .deleteRejected,
    ]

    /// Why a cull command is off (a folder opened in place never moves files); what
    /// New Session and Close Session do.
    public static func help(_ c: MenuCommand, model: AppModel) -> String? {
        switch c {
        case .newSession:
            model.canCreateSession ? "Create an empty session and open it (⇧⌘N)" : "This mode can’t create sessions"
        case .closeSession:
            if let s = model.currentSession {
                model.cull.busy ? "Wait until the files have moved"
                    : "Close “\(s.name)” (⌥⌘W). Nothing is deleted, and it stays in the Sessions list."
            } else {
                "No session is open"
            }
        case .moveRejectedToTrash, .movePickedToSelects, .restoreSessionTrash, .deleteRejected:
            model.cull.canMove ? nil : CullFinisher.inPlaceReason
        case .upscale:
            model.upscaler.map { $0.help(in: model) } ?? UpscaleModelDescriptor.reasons.missing
        default: nil
        }
    }

    /// Photo ▸ Enhance.
    public static let enhanceMenu: [MenuCommand] = [.upscale]

    /// Edit menu section (after Select All).
    public static let matchMenu: [MenuCommand] = [.matchExposures, .matchWhiteBalance]

    /// [multiview] View ▸ Filmstrip, and the strip's context menu (checked: the current one).
    public static let filmstripMenu: [MenuCommand] = [.filmstripBottom, .filmstripLeft, .filmstripRight]
    /// [multiview] View ▸ the filmstrip's thumbnail size (after the Filmstrip submenu).
    public static let filmstripSizeMenu: [MenuCommand] = [.largerFilmstripThumbnails, .smallerFilmstripThumbnails]

    /// Library menu section (after View's zoom items).
    public static let smartFolderMenu: [MenuCommand] = [
        .showAll, .showRating5, .showRating4, .showRating3, .showRating2, .showRating1,
    ]

    public static func isEnabled(_ c: MenuCommand, model: AppModel, layout: ToolLayout = .shared) -> Bool {
        let hasPhoto = model.focusedID != nil
        switch c {
        case .openFolder, .importCard, .cheatSheet, .fullScreen, .customizeTools, .voiceCommands, .showHistory, .voiceHelp,
             .showSelectionInViewer, .filmstripBottom, .filmstripLeft, .filmstripRight: return true
        // Shown as a checkmark; hiding it is off only when it is the last tab.
        case .toggleAllToolsTab: return !layout.workspace.contains(.allTools) || layout.workspace.canRemoveTab(.allTools)
        case .newSession: return model.canCreateSession && model.sheet == nil
        case .closeSession: return model.canCloseSession
        case .copyAdjustments: return model.focusedPhoto.map { !$0.isVideo } ?? false // videos have no adjustments
        case .copyFromPrevious, .autoAdjust, .rotateLeft, .rotateRight,
             .zoomIn, .zoomOut, .zoomFit, .zoomActual: return hasPhoto
        case .applyAdjustments: return model.canApply
        case .aiOptimize: return model.canOptimize || model.optimizePanel != nil
        case .aiCull: return !model.visibleIDs.isEmpty && model.cullProgress == nil
        case .askAI: return model.focusedPhoto.map { !$0.isVideo } ?? false
        case .find: return model.find.isAvailable
        case .showAIChanges: return !model.aiChangedPanels.isEmpty
        case .clearAIHighlights: return model.canClearAIHighlights
        case .aiChanges: return model.aiChangesShown || model.focusedPhoto.map { !$0.isVideo } ?? false
        case .undoAllAIChanges: return model.canUndoAllAIChanges
        case .moveTabUp: return layout.canMove(ToolWorkspaceCommands.selectedTab(model: model, layout: layout), by: -1)
        case .moveTabDown: return layout.canMove(ToolWorkspaceCommands.selectedTab(model: model, layout: layout), by: 1)
        case .showAll: return model.currentSession != nil
        case .exportSelected: return model.selectedIDs.contains { model.photo($0)?.url != nil }
        case .showRating1, .showRating2, .showRating3, .showRating4, .showRating5:
            return model.smartFolders.ratingFolder(c.ratingFolder!) != nil
        case .moveRejectedToTrash: return model.cull.canMove && !model.cull.busy && !model.cull.rejectedIDs.isEmpty
        case .movePickedToSelects: return model.cull.canMove && !model.cull.busy && !model.cull.selectIDs.isEmpty
        // Set-aside variants are only known on disk (N10): the command says when there is nothing.
        case .restoreSessionTrash: return model.cull.canMove && !model.cull.busy
        case .deleteRejected:
            return model.cull.canMove && !model.cull.busy && !(model.cull.rejectedIDs.isEmpty && model.cull.trashedIDs.isEmpty)
        case .newVariant: return (model.focusedPhoto.map { !$0.isVideo } ?? false) && model.sessions is any SessionFileOrganizing
        case .survey: return model.survey.isActive || model.surveyCandidates.count >= 2
        case .matchExposures, .matchWhiteBalance: return model.canMatchSet
        case .upscale: return model.upscaler.map { $0.isEnabled(in: model) } ?? false
        case .mergeHDR, .mergePanorama: return model.canMergePhotos
        // The viewer's strip only (the map's keeps its size); off at the end of the range.
        case .largerFilmstripThumbnails:
            return model.browserMode == .filmstrip && model.filmstrip.cell < FilmstripSizing.cellHeights.upperBound
        case .smallerFilmstripThumbnails:
            return model.browserMode == .filmstrip && model.filmstrip.cell > FilmstripSizing.cellHeights.lowerBound
        }
    }

    @discardableResult
    public static func perform(_ c: MenuCommand, model: AppModel, layout: ToolLayout = .shared) -> MenuCommand.Effect {
        switch c {
        case .openFolder:
            guard model.sheet == nil else { return .none } // never over a sheet
            return model.beginOpen() ? .none : .chooseFolder
        case .importCard: model.sheet = .importCard(nil)
        case .newSession: model.beginNewSession()
        case .closeSession: Task { await model.closeSession() }
        case .copyAdjustments: model.perform(.copyAdjustments)
        case .applyAdjustments: model.perform(.applyAdjustments)
        case .copyFromPrevious: model.perform(.copyFromPrevious)
        case .autoAdjust: model.perform(.auto)
        case .aiOptimize: if model.optimizePanel != nil { model.endOptimize() } else { model.beginOptimize() }
        case .aiCull: Task { await model.runAICull() }
        case .askAI: model.commandBar.present()
        case .find: model.find.present()
        case .showAIChanges: ToolWorkspaceCommands.revealAIChanges(model: model, layout: layout)
        case .clearAIHighlights: model.clearAIHighlights()
        case .aiChanges: model.toggleAIChangesList()
        case .undoAllAIChanges: Task { await model.undoAllAIChanges() }
        case .zoomIn: model.viewer.zoomIn()
        case .zoomOut: model.viewer.zoomOut()
        case .zoomFit: model.viewer.zoom(to: .fit)
        case .zoomActual: model.viewer.zoom(to: .actual)
        case .rotateLeft: model.viewer.rotate(by: -1)
        case .rotateRight: model.viewer.rotate(by: 1)
        case .fullScreen: model.viewer.state.fullScreen.toggle()
        case .cheatSheet: model.perform(.showCheatSheet)
        case .moveTabUp: layout.move(ToolWorkspaceCommands.selectedTab(model: model, layout: layout), by: -1)
        case .moveTabDown: layout.move(ToolWorkspaceCommands.selectedTab(model: model, layout: layout), by: 1)
        case .showAll: model.selectSmartFolder(nil)
        case .exportSelected: model.beginExport()
        case .showRating1, .showRating2, .showRating3, .showRating4, .showRating5:
            model.showRatingFolder(c.ratingFolder!)
        case .moveRejectedToTrash: Task { await model.cull.moveRejectedToTrash() }
        case .movePickedToSelects: model.cull.requestMoveToSelects()
        case .restoreSessionTrash: Task { await model.cull.restoreFromTrash() }
        case .deleteRejected: model.cull.requestDeleteRejected()
        case .newVariant: Task { await model.cull.newVariant() }
        case .survey: if model.survey.isActive { model.survey.finish() } else { model.beginSurvey() }
        case .matchExposures: model.matchTotalExposures()
        case .matchWhiteBalance: model.matchWhiteBalanceToFocused()
        case .customizeTools: return .openCustomizeTools
        case .toggleAllToolsTab: ToolWorkspaceCommands.toggleAllToolsTab(model: model, layout: layout)
        case .showHistory: ToolWorkspaceCommands.reveal(.history, model: model, layout: layout)
        case .showSelectionInViewer: model.settings.showSelectionInViewer.toggle()
        case .filmstripBottom, .filmstripLeft, .filmstripRight: model.placeFilmstrip(c.filmstripPosition!)
        case .largerFilmstripThumbnails: model.filmstrip = model.filmstrip.stepped(1)
        case .smallerFilmstripThumbnails: model.filmstrip = model.filmstrip.stepped(-1)
        case .voiceCommands: model.voice.toggle()
        case .voiceHelp: model.voice.helpShown.toggle()
        case .upscale: model.upscaler?.toggle()
        case .mergeHDR: model.beginMerge(.hdr)
        case .mergePanorama: model.beginMerge(.panorama)
        }
        return .none
    }

    /// Menu title for a plain-key command, showing the user's key (e.g. "Pick\tP").
    public static func title(_ c: BindableCommand, bindings: KeyBindings) -> String {
        MenuActions.title(c, bindings: bindings)
    }

    /// Plain-key commands in each menu, in order (`nil` = divider).
    public static let photoMenu: [BindableCommand?] = [
        .rate1, .rate2, .rate3, .rate4, .rate5, .rate0, nil, .pick, .reject, .unflag, nil,
        .tagRed, .tagYellow, .tagGreen, .tagBlue, .tagPurple, .tagOrange, .tagPink,
    ]
    public static let viewMenu: [BindableCommand] = [.gridMode, .viewerMode, .beforeAfter, .pinCompare, .togglePanels,
                                                      .toggleFilmstrip, .toggleStack]
    /// [cmp9] Photo ▸ composition, mask and picker tools. (Survey and the clipping warning
    /// keep their own menu items; their plain keys only add a second way in.)
    public static let toolMenu: [BindableCommand] = [.cropTool, .straightenTool, .colorPickerTool, .whiteBalanceTool,
                                                     .brushTool, .eraserTool, .gradientTool, .radialTool]

    /// Plain keys for commands whose menu item is a `MenuCommand` or a View toggle
    /// (Survey Selected ⌥⌘S, Clipping Warning ⌥⌘J): listed once, under that item.
    public static let keyOnly: [BindableCommand] = [.survey, .toggleClipping]

    public static func isEnabled(_ c: BindableCommand, model: AppModel) -> Bool {
        switch c {
        case .rate0, .rate1, .rate2, .rate3, .rate4, .rate5, .tagRed, .tagYellow, .tagGreen, .tagBlue, .tagPurple,
             .tagOrange, .tagPink, .pick, .reject, .unflag, .beforeAfter, .pinCompare, .toggleStack:
            model.focusedID != nil
        case .cropTool, .straightenTool, .colorPickerTool, .whiteBalanceTool, .brushTool, .eraserTool, .gradientTool,
             .radialTool:
            model.focusedPhoto.map { !$0.isVideo } ?? false
        case .survey:
            isEnabled(MenuCommand.survey, model: model)
        case .toggleClipping:
            true
        case .togglePanels, .gridMode, .viewerMode:
            true
        case .toggleFilmstrip:
            model.browserMode == .filmstrip
        }
    }
}

/// Tooltips that mention shortcuts, generated from the live bindings (N15).
@MainActor
public enum ToolbarHelp {
    /// `verifies`: the import service verifies copies (ds-import) [imp6].
    public static func importButton(mode: AppMode, verifies: Bool = false) -> String {
        if verifies {
            return "Import from a memory card or folder (⇧⌘I). Every file is copied, verified byte for byte, and never deleted from the card."
        }
        return mode == .live
            ? "Import is coming soon: Darkslide never copies from cards until it can verify every file; you can browse a card read-only (⇧⌘I)."
            : "Import from a memory card or folder (⇧⌘I). Demo: copies are simulated and not verified."
    }

    public static func importTitle(mode: AppMode, verifies: Bool = false) -> String {
        mode == .live && !verifies ? "Import (coming soon)" : "Import"
    }

    public static func viewerGrid(_ b: KeyBindings) -> String {
        "Viewer \(b.hint(.viewerMode)) or Grid \(b.hint(.gridMode))".replacingOccurrences(of: "  ", with: " ")
    }

    public static func beforeAfter(_ b: KeyBindings) -> String { b.help("Before / after split", .beforeAfter) }
    public static func compare(_ b: KeyBindings) -> String { b.help("Pin this photo and compare others against it", .pinCompare) }
    /// [multiview]
    public static let showSelection = "Show every selected photo in the viewer (⌘-click or ⇧-click in the filmstrip to select)"
    public static func pick(_ b: KeyBindings) -> String { b.help("Pick", .pick) }
    public static func reject(_ b: KeyBindings) -> String { b.help("Reject", .reject) }
    public static func stack(_ b: KeyBindings) -> String { b.help("Expand or collapse this burst stack", .toggleStack) }
    public static func autoAdjust() -> String { "Auto white balance and tone (⇧A)" }
    public static func aiOptimize() -> String {
        "AI Optimize: measured suggestions, each explained; choose which to apply (⌥⌘A)"
    }
    /// [cmp9] Crop / straighten toolbar buttons.
    public static func tool(_ t: CanvasTool, _ b: KeyBindings) -> String {
        switch t {
        case .crop: b.help("Crop: drag the handles; ⏎ applies, Esc cancels, X swaps orientation", .cropTool)
        case .straighten: b.help("Straighten: drag along something that should be level", .straightenTool)
        case .colorPicker: b.help("Color readout: click to pin the color under the cursor", .colorPickerTool)
        default: t.viewerLabel
        }
    }
}

/// The session commands' menu titles (shared by `CommandCatalog` and `MenuKeys`).
package enum CommandCatalogTitles {
    package static let newSession = "New Session…"
    package static let closeSession = "Close Session"
}
