import Foundation

/// One interactive control as VoiceOver and the audit test see it (N17).
public struct AccessibleControl: Equatable, Sendable, Identifiable {
    public var id: String
    public var label: String
    public var hint: String?

    public init(_ id: String, _ label: String, hint: String? = nil) {
        self.id = id
        self.label = label
        self.hint = hint
    }
}

/// Window toolbar controls. Views read their labels/hints from here, so the
/// audit test covers exactly what is on screen.
public enum ToolbarControl: String, CaseIterable, Sendable {
    case session, newSession, closeSession, importCard, viewerMode, gridMode, beforeAfter, compare, showSelection
    case undo, redo, copy, copyOptions, apply, auto, optimize, ask, aiCull, archive

    @MainActor
    public func control(_ model: AppModel) -> AccessibleControl {
        let b = model.settings.keyBindings
        switch self {
        case .session:
            return .init(rawValue, "Session: \(model.currentSession?.name ?? "none")",
                         hint: "Switch to another session, create one, or close this one")
        case .newSession:
            return .init(rawValue, "New session", hint: CommandCatalog.help(.newSession, model: model))
        case .closeSession:
            return .init(rawValue, model.currentSession.map { "Close session \($0.name)" } ?? "Close session",
                         hint: CommandCatalog.help(.closeSession, model: model))
        case .importCard:
            let verifies = model.importer is any ImportVerifying
            return .init(rawValue, ToolbarHelp.importTitle(mode: model.mode, verifies: verifies),
                         hint: ToolbarHelp.importButton(mode: model.mode, verifies: verifies))
        case .viewerMode: return .init(rawValue, "Viewer", hint: ToolbarHelp.viewerGrid(b))
        case .gridMode: return .init(rawValue, "Grid", hint: ToolbarHelp.viewerGrid(b))
        case .beforeAfter: return .init(rawValue, "Before and after", hint: ToolbarHelp.beforeAfter(b))
        case .compare:
            return .init(rawValue, model.viewer.state.comparePinned == nil ? "Compare" : "Stop comparing",
                         hint: ToolbarHelp.compare(b))
        case .showSelection: // [multiview]
            return .init(rawValue, model.settings.showSelectionInViewer ? "Showing selection in viewer" : "Show selection in viewer",
                         hint: ToolbarHelp.showSelection)
        case .undo:
            return .init(rawValue, model.history.undoLabel.map { "Undo \($0)" } ?? "Undo",
                         hint: model.history.canUndo ? "⌘Z" : "Nothing to undo")
        case .redo:
            return .init(rawValue, model.history.redoLabel.map { "Redo \($0)" } ?? "Redo",
                         hint: model.history.canRedo ? "⇧⌘Z" : "Nothing to redo")
        case .copy: return .init(rawValue, "Copy adjustments", hint: model.settings.copyMode == .edited ? "Copy what was edited on this photo (⇧⌘C)" : "Copy the ticked adjustments from this photo (⇧⌘C)")
        case .copyOptions: return .init(rawValue, "Copy options", hint: "Choose which adjustments Copy and Apply use")
        case .apply: return .init(rawValue, "Apply adjustments", hint: model.clipboardSummary)
        case .auto: return .init(rawValue, "Auto adjust", hint: ToolbarHelp.autoAdjust())
        case .optimize:
            let n = model.optimizeTargets.count
            return .init(rawValue, n > 1 ? "AI Optimize \(n) photos" : "AI Optimize", hint: ToolbarHelp.aiOptimize())
        case .ask:
            return .init(rawValue, "Edit with words",
                         hint: "Type an edit like “warmer and a bit brighter” (⌘K); ⌘Z undoes it")
        case .aiCull:
            if let p = model.cullProgress {
                return .init(rawValue, "AI Cull, \(p.done) of \(p.total)", hint: "Scoring on this Mac")
            }
            return .init(rawValue, "AI Cull", hint: "Suggest picks and rejects on this Mac; ⌘Z undoes it")
        case .archive:
            return .init(rawValue, ArchiveDisplay.accessibilityLabel(model.archiveStatus),
                         hint: ArchiveDisplay.explanation(model.archiveStatus.state))
        }
    }
}

/// Filter/sort bar controls (the bar lays out `FilterBarControl.all`).
public enum FilterBarControl: Hashable, Sendable {
    case star(Int), unrated, pick(PhotoFilter.PickFilter), tag(ColorTag), missing, search, clear, sort

    /// Controls shown for this state (Missing only when something is missing; Clear only when filtered).
    public static func all(filter: PhotoFilter, missingCount: Int, compact: Bool) -> [FilterBarControl] {
        var out: [FilterBarControl] = (1...5).map { .star($0) } + [.unrated]
        out += PhotoFilter.barPickOptions.map { .pick($0) }
        out += ColorTag.allCases.filter { $0 != .none }.map { .tag($0) }
        if missingCount > 0 || filter.missingOnly { out.append(.missing) }
        if !compact { out.append(.search) }
        if filter.isActive { out.append(.clear) }
        out.append(.sort)
        return out
    }

    public func control(filter: PhotoFilter, missingCount: Int, sort: PhotoSort) -> AccessibleControl {
        switch self {
        case .star(let n):
            let on = filter.minRating == n
            return .init("star\(n)", "\(n) star\(n == 1 ? "" : "s") or more",
                         hint: on ? "Selected; activate again to clear" : "Show photos rated \(n) or more")
        case .unrated: return .init("unrated", "Unrated", hint: "Only photos with no stars")
        case .pick(let p): return .init("pick-\(p.rawValue)", "Flag: \(p.rawValue)", hint: "Filter by pick flag")
        case .tag(let t):
            return .init("tag-\(t.name)", "\(t.name) tag", hint: "Only \(t.name.lowercased())-tagged photos")
        case .missing:
            return .init("missing", MissingActions.filterTitle(count: missingCount),
                         hint: "Photos whose files were moved or deleted outside Luxtable")
        case .search: return .init("search", "Search filenames", hint: nil)
        case .clear: return .init("clear", "Clear \(filter.activeCriteria) filter\(filter.activeCriteria == 1 ? "" : "s")")
        case .sort: return .init("sort", "Sort by \(sort.rawValue)", hint: "Change the sort order")
        }
    }

    /// What activating the control does to the filter.
    public func apply(to filter: inout PhotoFilter) {
        switch self {
        case .star(let n): filter.toggleMinRating(n)
        case .unrated: filter.toggleUnrated()
        case .pick(let p): filter.pick = p
        case .tag(let t): filter.toggleTag(t)
        case .missing: filter.missingOnly.toggle()
        case .clear: filter = PhotoFilter()
        case .search, .sort: break
        }
    }
}

/// Missing-originals UI copy and actions.
public enum MissingActions {
    public static func filterTitle(count: Int) -> String { "Missing (\(count.formatted()))" }

    public static func controls(selected: Int) -> [AccessibleControl] {
        [.init("remove", "Remove \(selected) missing photo\(selected == 1 ? "" : "s")",
               hint: "Forget these photos' ratings and edits. Files are never touched."),
         .init("locate", "Locate…", hint: "Point Luxtable at the moved file")]
    }

    /// Remove targets: the selected photos the store verified missing, on a drive that is
    /// connected (a photo whose drive dropped off is never removable).
    @MainActor
    public static func removable(_ model: AppModel) -> [PhotoID] {
        model.selectedIDs.filter(model.canRemoveMissing)
    }

    /// The confirmation before Remove: it forgets ratings and edits for good.
    public static func confirmRemove(count: Int) -> (title: String, message: String, button: String) {
        let photos = "\(count.formatted()) photo\(count == 1 ? "" : "s")"
        return ("Remove \(photos) from the session?",
                "This deletes ratings and edits for \(photos). The files themselves are never touched.",
                "Remove")
    }

    /// Selected missing photos whose only copy is inside the session store (R4-M5).
    @MainActor
    public static func restorable(_ model: AppModel) -> [PhotoID] {
        model.selectedIDs.filter { model.photo($0)?.copyInStore == true }
    }

    /// Selected "File changed" photos (R4-H3).
    @MainActor
    public static func changed(_ model: AppModel) -> [PhotoID] {
        model.selectedIDs.filter { model.photo($0)?.fileChanged == true }
    }

    public static let restoreToFolder = AccessibleControl(
        "restoreToFolder", "Restore to Folder",
        hint: "Move the only copy, kept inside Luxtable's session, back into the folder")
    public static let restorePrevious = AccessibleControl(
        "restorePrevious", "Restore Previous",
        hint: "This file replaced another photo: put back that photo's ratings and edits")
    public static let keepAsNew = AccessibleControl(
        "keepAsNew", "Keep as New", hint: "Treat the file as the new photo it is")

    public static func badge(_ p: Photo, missing: Bool = false) -> String? {
        if p.isMissing || missing { return "Missing" }
        if p.fileChanged { return "File changed" }
        return nil
    }
}

/// First-open progress copy (R3-M2).
public enum OpenProgressText {
    public static func title(folder: URL?) -> String { "Opening \(folder?.lastPathComponent ?? "folder")" }

    public static func phase(_ p: OpenProgress) -> String {
        switch p.phase {
        case .scanning: p.total > 0 ? "Scanning… \(p.done.formatted()) files" : "Scanning…"
        case .registering: "Registering \(p.done.formatted()) of \(p.total.formatted())"
        case .hashing:
            "Verifying files in the background… \(Int((p.fraction * 100).rounded()))% (\(p.done.formatted()) of \(p.total.formatted()))"
        case .updated: "Updated"
        }
    }

    /// The grid is usable while hashing continues: only scanning/registering block.
    public static func blocksBrowsing(_ p: OpenProgress) -> Bool { p.phase != .hashing }

    public static let cancel = AccessibleControl("cancelOpen", "Cancel opening", hint: "Stop; nothing is written to the folder")
}

/// Tool-tab strip and panel-header controls.
public enum ToolStripControl {
    /// `title`: the workspace's name for the tab (custom or renamed).
    public static func tab(_ t: ToolTab, title: String? = nil, selected: Bool) -> AccessibleControl {
        let name = title ?? t.title
        return .init("tab-\(t.rawValue)", "\(name) tools", hint: selected ? "Selected" : "Show the \(name) panels")
    }

    /// The strip's + (and an empty tab's Add Tool button).
    public static func addTool(tabTitle: String) -> AccessibleControl {
        .init("addtool", "Add or remove tools", hint: "Checked tools are in \(tabTitle); choosing one removes it")
    }

    /// The » menu holding the tabs that don't fit.
    public static func moreTabs(_ count: Int) -> AccessibleControl {
        .init("moretabs", "More tool tabs", hint: "\(count) more tab\(count == 1 ? "" : "s")")
    }

    /// A panel header's ⋯ menu.
    public static func panelMenu(_ p: ToolPanelID) -> AccessibleControl {
        .init("panelmenu-\(p.rawValue)", "\(p.title) options", hint: "Remove, move or add tools")
    }

    /// Arranging a tool within its tab (context menu items and VoiceOver actions).
    public static func arrange(_ p: ToolPanelID, tabTitle: String) -> [AccessibleControl] {
        [.init("moveup-\(p.rawValue)", "Move Up", hint: "Move \(p.title) up in \(tabTitle)"),
         .init("movedown-\(p.rawValue)", "Move Down", hint: "Move \(p.title) down in \(tabTitle)"),
         .init("remove-\(p.rawValue)", "Remove from \(tabTitle)", hint: "The Tools menu still reaches \(p.title)")]
    }

    public static func panelHeader(_ p: ToolPanelID, collapsed: Bool, favorite: Bool) -> [AccessibleControl] {
        var out = [AccessibleControl("disclose-\(p.rawValue)", PanelChrome.disclosureLabel(p.title, collapsed: collapsed),
                                     hint: collapsed ? "Expand" : "Collapse"),
                   .init("favorite-\(p.rawValue)", favorite ? "Remove \(p.title) from Favorites" : "Add \(p.title) to Favorites")]
        if let g = p.group {
            out += [.init("copy-\(p.rawValue)", "Copy \(g.title)", hint: "Copy only this tool's settings"),
                    .init("apply-\(p.rawValue)", "Apply \(g.title)", hint: "Apply only this tool's settings to the selection"),
                    .init("reset-\(p.rawValue)", "Reset \(g.title)")]
        }
        return out
    }
}

/// The AI Changes list's controls.
public enum AIChangesControl {
    public static let close = AccessibleControl("aiChangesClose", "Close AI Changes", hint: "Esc")

    @MainActor
    public static func undoAll(_ model: AppModel) -> AccessibleControl {
        .init("aiChangesUndoAll", model.undoAllAIChangesTitle,
              hint: "Put every setting AI changed back to its value before; ⌘Z brings them back")
    }

    /// A line: activating it shows the tool in the sidebar.
    public static func row(_ r: AIChangeRow) -> AccessibleControl {
        .init("aiChange-\(r.field.panel.rawValue)-\(r.field.name)", r.accessibilityLabel, hint: "Show it in the tools")
    }

    public static func undo(_ r: AIChangeRow) -> AccessibleControl {
        .init("aiChangeUndo-\(r.field.panel.rawValue)-\(r.field.name)", "Undo AI change to \(r.setting)",
              hint: r.canUndo ? "Put \(r.setting) back to its value before AI" : AIChangeText.cantUndo)
    }
}
