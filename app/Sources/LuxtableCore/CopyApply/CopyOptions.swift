import Foundation

/// Copy and Apply (PLAN §8). Copy takes the focused photo's adjustments
/// in one click and keeps them until the next Copy; Apply pastes them onto every selected
/// photo but the copied one, as one undo step. What is pasted is read from the Copy
/// options when Apply runs, so changing a tick after copying changes what Apply pastes.
extension AppModel {
    /// The groups Copy takes from `source`: its edited groups without the per-photo ones
    /// (`.edited`, Capture One's auto-select), or the user's own ticks (`.custom`).
    public func copiedGroups(from source: EditState) -> Set<AdjustGroup> {
        settings.copyMode == .edited ? AdjustGroup.changed(in: source).filter { !$0.isOptIn } : settings.copyGroups
    }

    /// What Apply pastes: the copied groups (relative where ticked) and the clipboard's chosen layers.
    public var pasteOptions: PasteOptions {
        let source = clipboard?.source ?? EditState()
        let layers = source.layers.map(\.id)
        let chosen = clipboardLayerIDs.intersection(layers)
        let groups = clipboard?.onlyGroup.map { [$0] } ?? copiedGroups(from: source)
        var o = PasteOptions(groups: groups, relative: settings.copyRelativeGroups.filter(\.supportsRelative))
        o.keepStraighten = settings.copyKeepsStraighten
        // All layers chosen = no filter.
        o.layerIDs = chosen.count == layers.count ? [] : chosen
        if !layers.isEmpty, chosen.isEmpty { o.groups.remove(.layers) }
        return o
    }

    /// The selected photos Apply pastes onto: never the copied photo itself (a relative
    /// group would add its change again, layers would be duplicated).
    public var applyTargets: [PhotoID] { selectedIDs.filter { $0 != clipboard?.sourceID } }

    /// How many photos Apply pastes onto: `applyTargets.count`, counted without building
    /// the ordered list (the toolbar asks on every selection change).
    var applyTargetCount: Int {
        let source = clipboard?.sourceID
        if selection.isEmpty { return focusedID == nil || focusedID == source || visibleIndex(of: focusedID!) == nil ? 0 : 1 }
        return selection.count { $0 != source && visibleIndex(of: $0) != nil } // a filter can hide selected photos
    }

    /// Apply has something to paste, somewhere to paste it, and no paste waiting.
    public var canApply: Bool {
        clipboard != nil && pendingPaste == nil && applyTargetCount > 0 && !pasteOptions.groups.isEmpty
    }

    /// Copy (⇧⌘C, the toolbar's Copy): the focused photo's adjustments, no dialog.
    public func copyWithOptions() {
        guard let p = focusedPhoto, !p.isVideo else { return }
        copyAdjustments()
        let groups = pasteOptions.groups
        let n = groups.count
        let message: String
        if n == 0 {
            let perPhoto = Self.perPhotoTitles(AdjustGroup.changed(in: edit(for: p.id)))
            message = settings.copyMode == .custom ? "Copied \(p.filename), but no adjustments are ticked: choose them in Copy’s options"
                : perPhoto.isEmpty ? "\(p.filename) has no edits to copy"
                : "\(p.filename) has only per-photo edits (\(perPhoto.joined(separator: ", "))): tick them in Copy’s Custom options to copy them"
        } else {
            let perPhoto = Self.perPhotoTitles(groups)
            message = "Copied \(n) adjustment\(n == 1 ? "" : "s") from \(p.filename)"
                + (perPhoto.isEmpty ? "" : ", including \(perPhoto.joined(separator: ", "))")
        }
        show(Toast(message, symbol: "arrow.up.doc"))
    }

    /// Titles of the per-photo (opt-in) groups in `groups`: named wherever they are pasted.
    static func perPhotoTitles(_ groups: Set<AdjustGroup>) -> [String] {
        AdjustGroup.allCases.filter { groups.contains($0) && $0.isOptIn }.map(\.title)
    }

    /// Apply (⇧⌘V, the toolbar's Apply): paste onto the selection as one undo step.
    /// Returns the number of photos changed (0 while as-shot values load for a relative white balance).
    @discardableResult
    public func applyToSelection() -> Int {
        guard let clipboard else {
            show(Toast("Copy adjustments first (⇧⌘C)", symbol: "doc.on.clipboard"))
            return 0
        }
        let options = pasteOptions
        guard !options.groups.isEmpty else {
            show(Toast("No adjustments to apply: choose them in Copy’s options", symbol: "doc.on.clipboard"))
            return 0
        }
        let targets = applyTargets
        guard !targets.isEmpty else {
            show(Toast("Select the photos to apply to: \(clipboard.sourceName ?? "the copied photo") is never pasted onto itself",
                       symbol: "doc.on.clipboard"))
            return 0
        }
        return pasteLoadingAsShotIfNeeded(to: targets, options: options)
    }

    /// The Apply button's tooltip: where the clipboard came from and what Apply pastes.
    /// What is on the clipboard, short, for the chip between Copy and Apply: the first two
    /// groups and how many more ("White Balance · Exposure +2"); nil when nothing is copied.
    public var clipboardChip: String? {
        guard clipboard != nil else { return nil }
        let groups = AdjustGroup.allCases.filter(pasteOptions.groups.contains).map(\.title)
        guard !groups.isEmpty else { return nil }
        let shown = groups.prefix(2).joined(separator: " · ")
        return groups.count > 2 ? "\(shown) +\(groups.count - 2)" : shown
    }

    public var clipboardSummary: String {
        guard let clipboard else { return "Copy adjustments first (⇧⌘C)" }
        if pendingPaste != nil { return "Loading the as-shot white balance for the last Apply…" }
        let from = clipboard.sourceName ?? "the copied photo"
        let n = applyTargetCount
        guard n > 0 else { return "Select the photos to apply to, then ⇧⌘V (\(from) is the copied photo: Apply never pastes onto it)" }
        let o = pasteOptions
        let groups = AdjustGroup.allCases.filter(o.groups.contains).map { o.isRelative($0) ? "\($0.title) (relative)" : $0.title }
        var what = groups.isEmpty ? "nothing to apply" : groups.joined(separator: ", ")
        if clipboard.onlyGroup != nil { what += " only (copied from its panel)" }
        var lines = ["Apply to \(MatchSummary.photos(n)) (⇧⌘V)", "From \(from): \(what)"]
        let perPhoto = Self.perPhotoTitles(o.groups)
        if !perPhoto.isEmpty {
            let keep = o.groups.contains(.crop) && o.keepStraighten ? " (each photo keeps its straighten)" : ""
            lines.append("Includes per-photo groups: \(perPhoto.joined(separator: ", "))\(keep)")
        }
        return lines.joined(separator: "\n")
    }
}

/// The Copy button's options (its ▾ popover): Edited (the source's edited groups, the
/// default) or Custom ticks, absolute or relative, keeping each photo's straighten, and
/// which of the clipboard's layers. Every change is kept at once: there is no Remember box.
@MainActor
public struct CopyOptions {
    public let app: AppModel

    public init(app: AppModel) { self.app = app }

    /// The photo the options describe: the clipboard's source once something is copied,
    /// else the focused photo (what Copy would take).
    public var sourceName: String? {
        if let c = app.clipboard { return c.sourceName }
        return app.focusedPhoto?.filename
    }
    public var source: EditState { app.clipboard?.source ?? app.focusedID.map(app.edit(for:)) ?? EditState() }
    /// Groups edited on the source (the dots, and what Edited takes).
    public var changedGroups: Set<AdjustGroup> { AdjustGroup.changed(in: source) }
    /// A panel header's Copy: Apply pastes just this group until the options change.
    public var onlyGroup: AdjustGroup? { app.clipboard?.onlyGroup }

    public var mode: CopyMode { app.settings.copyMode }
    /// What Copy takes (Edited: recomputed from the source; Custom: the ticks).
    public var groups: Set<AdjustGroup> { app.copiedGroups(from: source) }
    public var relative: Set<AdjustGroup> { app.settings.copyRelativeGroups }
    public var keepStraighten: Bool { app.settings.copyKeepsStraighten }

    /// A tick switches to Custom, starting from what was shown.
    public func toggle(_ g: AdjustGroup) { setCustom(groups.symmetricDifference([g])) }
    /// Custom (the picker): the saved Custom ticks come back. Only a row's tick made in
    /// Edited starts Custom from the ticks shown (`toggle`).
    public func selectCustom() {
        app.settings.copyMode = .custom
        app.clearClipboardOnlyGroup()
    }
    public func selectAll() { setCustom(Set(AdjustGroup.allCases)) }
    public func selectNone() { setCustom([]) }
    /// "Edited": the source's changed groups, without crop, heal & clone and layers (N6),
    /// recomputed on every Copy.
    public func selectEdited() {
        app.settings.copyMode = .edited
        app.clearClipboardOnlyGroup()
    }

    public func toggleRelative(_ g: AdjustGroup) {
        guard g.supportsRelative else { return }
        app.settings.copyRelativeGroups.formSymmetricDifference([g])
    }

    public func toggleKeepStraighten() { app.settings.copyKeepsStraighten.toggle() }

    /// The clipboard's layers (chosen per copy; all of them by default).
    public var layers: [LayerEdit] { app.clipboard?.source.layers ?? [] }
    public func isLayerOn(_ id: Int) -> Bool { app.clipboardLayerIDs.contains(id) }
    public func toggleLayer(_ id: Int) { app.clipboardLayerIDs.formSymmetricDifference([id]) }

    private func setCustom(_ g: Set<AdjustGroup>) {
        app.settings.copyGroups = g
        app.settings.copyMode = .custom
        app.clearClipboardOnlyGroup()
    }
}
