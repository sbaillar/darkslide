import CoreGraphics
import Foundation
import Observation

extension AppModel {
    // MARK: - Culling metadata (undoable)

    /// Apply `change` to every photo in `ids` as one undo step.
    public func updatePhotos(_ ids: [PhotoID], label: String, _ change: (inout Photo) -> Void) {
        guard !refusingEdits() else { return }
        noteUserActivity()
        var step = HistoryStep(label: label)
        var changed: [Photo] = []
        for id in ids {
            guard var p = items[id]?.photo else { continue }
            let before = p
            change(&p)
            guard p != before else { continue }
            step.photosBefore[id] = before
            step.photosAfter[id] = p
            changed.append(p)
        }
        guard !changed.isEmpty else { return }
        applyPhotos(step.photosAfter)
        history.record(step)
        persist(photos: changed)
    }

    /// People wrote tags to the sidecars: show them (`ids` not listed have
    /// nobody). Not an undo step; People has its own Remove.
    func setPeople(_ people: [PhotoID: [String]]) {
        var updated: [PhotoID: Photo] = [:]
        for (id, item) in items where item.photo.people != (people[id] ?? []) {
            var p = item.photo
            p.people = people[id] ?? []
            updated[id] = p
        }
        applyPhotos(updated)
    }

    /// Mutate only the changed items; re-filter/re-sort only when that can change the result.
    func applyPhotos(_ updated: [PhotoID: Photo]) {
        guard !updated.isEmpty else { return }
        var affectsFilter = false
        var affectsSort = false
        for (id, p) in updated {
            guard let item = items[id] else { continue }
            let old = item.photo
            if filter.isActive || old.isMissing || p.isMissing, filter.matches(old) != filter.matches(p) { affectsFilter = true }
            if sort == .rating, old.rating != p.rating { affectsSort = true }
            smartFolders.photoChanged(old: old, new: p) // O(folders) count delta
            item.photo = p
        }
        photosVersion += 1
        if affectsSort {
            resort()
        } else if affectsFilter {
            if updated.count <= 64 { patchVisible(Array(updated.keys)) } else { rebuildVisible() }
        }
    }

    // Show only `id`'s members (`nil` = all photos, ⌘0).
    // Replaces the other filters so `visibleIDs` is exactly the folder.
    // The typed search text stays.
    public func selectSmartFolder(_ id: Int64?) {
        filter = PhotoFilter(text: filter.text, smartFolder: id.flatMap { smartFolders.scope($0) })
    }

    /// After folders reload (edit, delete, keyword index), the
    /// selected folder's rules are re-read; a folder that's gone is deselected.
    func refreshSmartFolderScope() {
        guard let old = filter.smartFolder else { return }
        filter.smartFolder = smartFolders.refreshed(old)
    }

    /// ⌘1…⌘5: the built-in "exactly `n` stars" folder.
    public func showRatingFolder(_ n: Int) {
        selectSmartFolder(smartFolders.ratingFolder(n)?.id)
    }

    public func selectAll() { selection = Set(visibleIDs) }

    /// After rating/flagging a single photo, move to the next one (C1/LR culling flow).
    /// `focusBefore`: focus when the key was pressed. If the change itself moved
    /// focus (the photo left the filter and its neighbour took its place), that
    /// already was the advance.
    func autoAdvanceIfNeeded(focusBefore: PhotoID?) {
        refocusIntoNavigation()
        guard settings.autoAdvance, selection.count <= 1, focusedID == focusBefore else { return }
        move(.right, extend: false)
    }

    public func setRating(_ r: Int) {
        let rating = min(max(r, 0), 5)
        let before = focusedID
        updatePhotos(selectedIDs, label: rating == 0 ? "Clear Rating" : "Rate \(rating)") { $0.rating = rating }
        autoAdvanceIfNeeded(focusBefore: before)
    }

    /// Toggles: pressing the key of the current tag clears it (Lightroom behaviour).
    public func setTag(_ tag: ColorTag) {
        let ids = selectedIDs
        let allHave = ids.allSatisfy { photo($0)?.colorTag == tag }
        let value = allHave ? .none : tag
        updatePhotos(ids, label: "Color Tag \(value.name)") { $0.colorTag = value }
    }

    public func setPick(_ state: PickState) {
        let label = switch state {
        case .picked: "Pick"
        case .rejected: "Reject"
        case .none: "Unflag"
        }
        let before = focusedID
        updatePhotos(selectedIDs, label: label) { $0.pick = state }
        autoAdvanceIfNeeded(focusBefore: before)
    }

    func persist(photos changed: [Photo]) {
        saver.enqueue(photos: changed)
        noteArchiveChanges(changed.count)
    }

    // MARK: - Edits (undoable)

    /// Mutate the focused edit (or every selected one with `editAllSelected`).
    /// Calls with the same `coalesceKey` in quick succession (a slider drag) form one undo step.
    public func updateEdit(label: String, coalesceKey: String? = nil, _ mutate: (inout EditState) -> Void) {
        guard let f = focusedID else { return }
        var ids = [f]
        if editAllSelected {
            // Only global adjustments sync to the selection; layer structure,
            // masks, crop, straighten and rotation stay per image.
            var probe = edit(for: f)
            let before = probe
            mutate(&probe)
            let perImage: [AdjustGroup] = [.layers, .crop]
            if !perImage.contains(where: { $0.differs(before, probe) }) {
                ids = selectedIDs.contains(f) ? selectedIDs : [f] + selectedIDs
            }
        }
        updateEdits(ids, label: label, coalesceKey: coalesceKey) { _, e in mutate(&e) }
    }

    /// Run `body` for the open session: it is cancelled when another session opens, and
    /// `isCurrent` tells it whether its session is still the open one (check it after
    /// every await, before applying results).
    @discardableResult
    func sessionBound(_ body: @escaping @MainActor (_ isCurrent: @escaping @MainActor () -> Bool) async -> Void)
        -> Task<Void, Never> {
        let key = UUID()
        let session = currentSession?.id
        let t = Task { @MainActor [weak self] in
            await body { [weak self] in !Task.isCancelled && self?.currentSession?.id == session }
            self?.sessionBoundTasks[key] = nil
        }
        sessionBoundTasks[key] = t
        return t
    }

    /// Apply per-photo edit changes as one undo step. `ai`: an AI edit, whose changed rows
    /// the tool sidebar highlights (any other step clears the highlights of rows it changes).
    public func updateEdits(_ ids: [PhotoID], label: String, coalesceKey: String? = nil,
                            reports: [PhotoID: ReportChange] = [:], ai: AIMarks.Source? = nil,
                            _ mutate: (PhotoID, inout EditState) -> Void) {
        var step = HistoryStep(label: label, coalesceKey: coalesceKey)
        for id in ids where photo(id)?.isVideo != true { // videos aren't editable
            let before = edit(for: id)
            var after = before
            mutate(id, &after)
            guard after != before else { continue }
            step.editsBefore[id] = before
            step.editsAfter[id] = after
        }
        guard !step.editsAfter.isEmpty else { return }
        step.reports = reports.filter { step.editsAfter[$0.key] != nil }
        step.marks = marksChanges(step, ai: ai)
        // One row per photo per step (Edit All Selected adds one to each). A row
        // merges only when its photo is already in the open step.
        let open = history.merges(step) ? history.undoStack.last : nil
        for (id, edit) in step.editsAfter {
            let rows = photoHistories[id] ?? PhotoHistory()
            var next = rows
            let after = HistoryState(edit: edit,
                                     report: step.reports[id].map(\.after) ?? optimizeReports[id],
                                     marks: step.marks[id].map(\.after) ?? aiMarks[id])
            next.record(label: label, before: historyState(id), after: after, coalesceKey: coalesceKey,
                        merging: open?.histories[id] != nil)
            step.histories[id] = HistoryChange(before: rows, after: next)
        }
        commit(step)
    }

    /// Apply a built step (its reports, highlights, edits and History rows) and record it
    /// as one undo step.
    func commit(_ step: HistoryStep) {
        guard !refusingEdits() else { return }
        applyReports(step.reports.mapValues(\.after))
        applyMarks(step.marks.mapValues(\.after))
        // A coalesced change is a gesture (a slider drag): the viewer drafts until it ends.
        if step.coalesceKey != nil { viewer.beginEditGesture() }
        if !step.editsAfter.isEmpty { applyEdits(step.editsAfter) }
        applyHistories(step.histories.mapValues(\.after), edited: step.editsAfter.keys)
        history.record(step)
    }

    /// Set photos' rows. A photo whose edit changed in a step that no longer has
    /// its rows (they were cleared since) shows its new state as its one "Current edit" row.
    func applyHistories(_ updated: [PhotoID: PhotoHistory], edited: some Collection<PhotoID>) {
        for (id, h) in updated { photoHistories[id] = h.isEmpty ? nil : h }
        for id in edited where updated[id] == nil && photoHistories[id] != nil {
            photoHistories[id] = .cleared(historyState(id))
        }
        dropStaleHistoryPreview()
    }

    func applyEdits(_ updated: [PhotoID: EditState]) {
        // Any edit, undo or redo while AI Optimize is open would leave its preview stale
        // and be overwritten by Apply: the panel closes instead (its Apply has already
        // closed it).
        if optimizePanel != nil { endOptimize() }
        for (id, e) in updated { edits[id] = e.isDefault ? nil : e }
        var edited: [PhotoID: Photo] = [:]
        for id in updated.keys {
            if var p = photo(id), p.isEdited != !(edits[id]?.isIdentity ?? true) {
                p.isEdited.toggle()
                edited[id] = p
            }
        }
        if !edited.isEmpty { applyPhotos(edited) }
        saver.enqueue(edits: updated)
        noteArchiveChanges(updated.count)
        viewer.editsChanged(Set(updated.keys))
    }

    /// A slider drag or canvas gesture ended: close its undo step and save now.
    public func endEditGesture() {
        history.endGesture()
        viewer.endEditGesture()
        Task { await saver.flush() }
    }

    public func resetGroup(_ group: AdjustGroup) {
        updateEdit(label: "Reset \(group.title)") { e in
            e = AdjustmentClipboard(source: EditState()).apply(to: e, options: PasteOptions(groups: [group]))
            if group == .layers { e.layers = [] }
        }
    }

    public func undo() {
        // Right after "Move Rejected to Trash"/"to Selects", ⌘Z moves them back.
        if let busy = cull.pendingUndo {
            Task { [weak self] in
                await busy.value
                self?.undo()
            }
            return
        }
        if cull.undoesMove(historyDepth: history.undoStack.count) {
            cull.startUndoLastMove()
            return
        }
        guard !refusingEdits(), let step = history.undo() else { return }
        applyEdits(step.editsBefore)
        applyReports(step.reports.mapValues(\.before))
        applyMarks(step.marks.mapValues(\.before))
        applyHistories(step.histories.mapValues(\.before), edited: step.editsBefore.keys)
        applyPhotos(step.photosBefore)
        if !step.photosBefore.isEmpty { persist(photos: Array(step.photosBefore.values)) }
        show(Toast("Undo \(step.label)", symbol: "arrow.uturn.backward"))
    }

    public func redo() {
        guard !refusingEdits(), let step = history.redo() else { return }
        applyEdits(step.editsAfter)
        applyReports(step.reports.mapValues(\.after))
        applyMarks(step.marks.mapValues(\.after))
        applyHistories(step.histories.mapValues(\.after), edited: step.editsAfter.keys)
        applyPhotos(step.photosAfter)
        if !step.photosAfter.isEmpty { persist(photos: Array(step.photosAfter.values)) }
        show(Toast("Redo \(step.label)", symbol: "arrow.uturn.forward"))
    }

    // MARK: - Copy / Apply

    /// Copy `id` (else the focused photo) to the clipboard, with what Apply will need of the
    /// source (its name, size and as-shot white balance: ids are per session). `only`: a
    /// tool panel header's Copy, so Apply pastes just that group until the next Copy.
    public func copyAdjustments(from id: PhotoID? = nil, only: AdjustGroup? = nil) {
        guard let source = id ?? focusedID, photo(source)?.isVideo != true else { return }
        let p = photo(source)
        clipboard = AdjustmentClipboard(source: edit(for: source), sourceID: source, sourceName: p?.filename,
                                        sourceSize: p?.pixelSize, sourceAsShot: asShot(for: source), onlyGroup: only)
        clipboardLayerIDs = Set(edit(for: source).layers.map(\.id))
        // A relative white balance needs the source's as-shot values: load them now (only
        // then: it decodes a RAW never rendered).
        if clipboard?.sourceAsShot == nil, tools != nil, settings.copyRelativeGroups.contains(.whiteBalance) {
            sessionBound { [weak self] isCurrent in
                await self?.loadDevelopInfo(for: [source])
                guard let self, isCurrent(), self.clipboard?.sourceID == source, self.clipboard?.sourceAsShot == nil else { return }
                self.clipboard?.sourceAsShot = self.asShot(for: source)
            }
        }
    }

    /// The Copy options changed: a panel header's single-group copy becomes a normal one.
    func clearClipboardOnlyGroup() { clipboard?.onlyGroup = nil }

    /// Photos whose as-shot white balance a relative WB paste needs but that
    /// aren't loaded yet (the source, unless captured at Copy, and the targets).
    func missingAsShot(for targets: [PhotoID], options: PasteOptions, clipboard clip: AdjustmentClipboard? = nil) -> [PhotoID] {
        guard tools != nil, let clip = clip ?? clipboard, options.groups.contains(.whiteBalance),
              options.relative.contains(.whiteBalance) else { return [] }
        let source = clip.sourceAsShot == nil ? [clip.sourceID].compactMap { $0 } : []
        return (source + targets).filter { developInfo[$0] == nil && photo($0)?.isVideo != true }
    }

    /// Paste now when nothing needs loading (returns the number changed), else load the
    /// as-shot values and paste in `pendingPaste` (returns 0 now). The clipboard and options
    /// are the ones of the press; the paste is dropped if the session closes or changes
    /// meanwhile, and a second press while one waits is ignored (it would paste twice).
    @discardableResult
    func pasteLoadingAsShotIfNeeded(to targets: [PhotoID], options: PasteOptions) -> Int {
        guard var clip = clipboard else { return 0 }
        guard pendingPaste == nil else {
            show(Toast("Still loading the as-shot white balance for the last Apply…", symbol: "hourglass"))
            return 0
        }
        if clip.sourceAsShot == nil { clip.sourceAsShot = clip.sourceID.flatMap(asShot(for:)) }
        let missing = missingAsShot(for: targets, options: options, clipboard: clip)
        guard !missing.isEmpty else { return applyClipboard(to: targets, options: options, clipboard: clip) }
        show(Toast("Loading the as-shot white balance of \(MatchSummary.photos(missing.count))…", symbol: "hourglass"))
        let session = currentSession?.id
        pendingPaste = Task { [weak self, clip] in
            guard let self else { return 0 }
            await self.loadDevelopInfo(for: missing)
            guard !Task.isCancelled else { return 0 } // closing the session cleared `pendingPaste`
            self.pendingPaste = nil
            guard self.currentSession?.id == session else { return 0 }
            var pressed = clip
            if pressed.sourceAsShot == nil { pressed.sourceAsShot = pressed.sourceID.flatMap(self.asShot(for:)) }
            return self.applyClipboard(to: targets, options: options, clipboard: pressed)
        }
        return 0
    }

    /// Paste `clipboard` (default: the current one) onto `targets` as one undo step.
    /// Returns the number changed.
    @discardableResult
    public func applyClipboard(to targets: [PhotoID], options: PasteOptions, clipboard clip: AdjustmentClipboard? = nil) -> Int {
        guard let clipboard = clip ?? clipboard else { return 0 }
        // Counted by comparing edits: the undo depth stops growing at the history limit.
        let before = Dictionary(targets.map { ($0, edit(for: $0)) }) { a, _ in a }
        // Relative WB reads each side's As Shot through its real as-shot values.
        let sourceAsShot = clipboard.sourceAsShot
        // A crop is re-fitted to each target's pixels (ds-core `adapt_crop`).
        let sourceSize = clipboard.sourceSize
        let sizes = Dictionary(targets.compactMap { id in photo(id).map { (id, $0.pixelSize) } }) { a, _ in a }
        var wbAsIs = 0
        updateEdits(targets, label: "Apply Adjustments") { [developInfo] id, e in
            if options.isRelative(.whiteBalance), options.groups.contains(.whiteBalance),
               AdjustmentClipboard.relativeWhiteBalanceFallsBack(source: clipboard.source.whiteBalance, target: e.whiteBalance,
                                                                 sourceAsShot: sourceAsShot, targetAsShot: developInfo[id]?.asShot) {
                wbAsIs += 1
            }
            e = clipboard.apply(to: e, options: options, sourceAsShot: sourceAsShot, targetAsShot: developInfo[id]?.asShot,
                                sourceSize: sourceSize, targetSize: sizes[id])
        }
        let changedIDs = before.keys.filter { edit(for: $0) != before[$0] }
        let changed = changedIDs.count
        var message = changed == 0 ? "Nothing to apply" : "Applied to \(MatchSummary.photos(changed))"
        // Relative white balance without as-shot values (demo mode, no camera data) is absolute.
        if changed > 0, wbAsIs > 0 { message += "; white balance pasted as is on \(MatchSummary.photos(wbAsIs)) (as-shot unknown)" }
        let misfits = profileMisfits(in: changedIDs)
        show(Toast(message + Self.profileMisfitNote(misfits),
                   symbol: misfits > 0 || wbAsIs > 0 ? "exclamationmark.triangle" : "doc.on.clipboard"))
        return changed
    }

    /// How many of `ids` now use an imported camera profile made for another camera
    /// (it renders as Darkslide Standard on them, ds-pipeline).
    func profileMisfits(in ids: [PhotoID]) -> Int {
        ids.count { id in
            guard let p = photo(id) else { return false }
            let profile = edit(for: id).profile
            if case .dcp = profile { return !CameraProfileLibrary.fits(profile, camera: p.meta.camera) }
            return false
        }
    }

    /// The warning appended to a toast when a profile doesn't fit some photos' camera.
    static func profileMisfitNote(_ n: Int) -> String {
        n == 0 ? "" : "; the camera profile is for another camera on \(MatchSummary.photos(n)) (shown with Darkslide Standard)"
    }

    /// The profile picker: set the profile (on the selection with Edit All), warning when
    /// it doesn't fit some of the photos' cameras.
    public func setProfile(_ profile: CameraProfileRef) {
        let ids = Set(selectedIDs + (focusedID.map { [$0] } ?? []))
        updateEdit(label: "Profile") { $0.profile = profile }
        let misfits = profileMisfits(in: ids.filter { edit(for: $0).profile == profile })
        if misfits > 0 {
            show(Toast("\(profile.displayName) is for another camera: \(MatchSummary.photos(misfits)) shown with Darkslide Standard",
                       symbol: "exclamationmark.triangle"))
        }
    }

    /// ⌥⌘V: copy every adjustment from the previous image onto the selection.
    public func copyFromPrevious() {
        guard let f = focusedID, let i = visibleIndex(f), i > 0 else { return }
        let prev = visibleIDs[i - 1]
        let source = AdjustmentClipboard(source: edit(for: prev), sourceID: prev)
        let targets = selectedIDs
        let sourceSize = photo(prev)?.pixelSize
        let sizes = Dictionary(targets.compactMap { id in photo(id).map { (id, $0.pixelSize) } }) { a, _ in a }
        updateEdits(targets, label: "Copy from Previous") { id, e in
            // The paste keeps each photo's orientation.
            e = source.apply(to: e, options: PasteOptions(), sourceSize: sourceSize, targetSize: sizes[id])
        }
        let misfits = profileMisfits(in: targets)
        show(Toast("Copied from \(photo(prev)?.filename ?? "previous")" + Self.profileMisfitNote(misfits),
                   symbol: misfits > 0 ? "exclamationmark.triangle" : "doc.on.doc"))
    }
}
