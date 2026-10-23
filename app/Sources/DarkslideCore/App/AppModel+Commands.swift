import CoreGraphics
import Foundation
import Observation

extension AppModel {
    // MARK: - Auto and AI

    public func autoAdjust() async {
        guard let p = focusedPhoto, !p.isVideo else { return }
        let session = currentSession?.id
        do {
            let base = edit(for: p.id)
            let estimate = try await ai.autoAdjust(p, current: base, images: images)
            // Live mode: white balance and tone from the camera data on the GPU.
            let adjusted = try await developAuto(p, base: base, estimate: estimate)
            guard isStill(in: session) else { return } // another session is opening or opened: its ids differ
            // Apply only what the AI changed, on top of edits made while it ran.
            updateEdits([p.id], label: "Auto Adjust", ai: .auto) { _, e in e = AIEditMerge.apply(result: adjusted, base: base, current: e) }
            // Say what changed, like the ⌘K summary ("Auto: +0.4 EV, 5235→5208 K").
            let summary = InstructionSummary.describe(before: base, after: adjusted, asShot: asShot(for: p.id))
            show(Toast(summary.isEmpty ? "Auto white balance and tone applied" : "Auto: \(summary)", symbol: "wand.and.stars"))
        } catch {
            show(error)
        }
    }

    public func applyInstruction(_ text: String) async {
        await runInstruction(text) // guarded parser, straighten, toast (AI/AppModel+Instruction.swift)
    }

    /// Score the visible photos and apply picks/rejects as one undo step.
    public func runAICull() async {
        let targets = visiblePhotos.filter { !$0.isVideo } // clips aren't culled by AI
        guard !targets.isEmpty, cullProgress == nil else { return }
        cullProgress = (0, targets.count)
        let session = currentSession?.id
        let ai = ai, images = images
        let task = Task {
            try await ai.cull(targets, images: images) { done, total in
                Task { @MainActor [weak self] in
                    if self?.cullProgress != nil { self?.cullProgress = (done, total) }
                }
            }
        }
        cullTask = task
        defer {
            if cullTask == task {
                cullProgress = nil
                cullTask = nil
            }
        }
        do {
            let suggestions = try await task.value
            guard !task.isCancelled, isStill(in: session) else { return } // the session closed or changed meanwhile
            cullSuggestions = Dictionary(suggestions.map { ($0.photoID, $0) }, uniquingKeysWith: { a, _ in a })
            browserState.bestHints = Set(cullSuggestions.filter { $0.value.pick == .picked }.map(\.key))
            // Only photos the user hasn't decided on (no flag, rating or colour tag)
            // change, and the toast counts the changes actually made.
            let plan = AICullApplication.plan(targets.compactMap { photo($0.id) }, suggestions: cullSuggestions)
            if !plan.changes.isEmpty {
                updatePhotos(Array(plan.changes.keys), label: "AI Cull") { p in
                    if let pick = plan.changes[p.id] {
                        AICullApplication.apply(pick, note: cullSuggestions[p.id]?.rejectNote, to: &p)
                    }
                }
            }
            show(Toast(plan.message, symbol: "sparkles"))
        } catch is CancellationError {
        } catch {
            show(error)
        }
    }

    // MARK: - Keyboard

    public func setKeyRepeat(_ active: Bool) {
        guard active != fastNavigation else { return }
        fastNavigation = active
        viewer.setFastNavigation(active)
    }

    /// Route one key press through the user's bindings (the monitor's entry point).
    @discardableResult
    public func handleKey(_ key: KeyInput, focus: FocusedResponder, sheetOpen: Bool, isMainWindow: Bool) -> Bool {
        // AI Optimize owns Esc (close), ⏎ (Apply) and Tab (into the panel) while it is open,
        // before survey mode and the viewer tools.
        if let panel = optimizePanel, isMainWindow, focus != .text, !(sheetOpen || sheet != nil),
           !key.command, !key.option, !key.control {
            switch key.special {
            case .escape: endOptimize(); return true
            case .returnKey: panel.apply(); return true
            case .tab: return false
            default: break
            }
        }
        // Survey mode owns X, U, the arrows, ⏎ and Esc while it is up.
        if survey.isActive, isMainWindow, focus != .text, !(sheetOpen || sheet != nil), handleSurveyKey(key) { return true }
        // The crop/straighten tools own ⏎ (commit), Esc (cancel) and X (swap).
        if isMainWindow, focus != .text, !(sheetOpen || sheet != nil),
           focus != .control || key.special == .escape, viewer.handleToolKey(key) { return true }
        guard let cmd = KeyMonitorPolicy.shouldHandle(key, focus: focus, sheetOpen: sheetOpen || sheet != nil,
                                                      isMainWindow: isMainWindow, bindings: settings.keyBindings)
        else { return false }
        if case .move = cmd { setKeyRepeat(key.isRepeat) }
        perform(cmd)
        return true
    }

    public func perform(_ command: KeyCommand) {
        switch command {
        case .move(let d, let extend): move(d, extend: extend)
        case .rate(let n): if browserMode == .library { library.rate(n) } else { setRating(n) } // [library]
        case .tag(let t): if browserMode == .library { library.setLabel(t) } else { setTag(t) }
        case .pick: if browserMode == .library { library.setFlag(.picked) } else { setPick(.picked) }
        case .reject: if browserMode == .library { library.setFlag(.rejected) } else { setPick(.rejected) }
        case .unflag: if browserMode == .library { library.setFlag(.none) } else { setPick(.none) }
        case .zoomToggleAtCursor:
            if let p = focusedPhoto, p.isVideo, let url = p.url { VideoQuickLook.shared.toggle(url) }
            else if browserMode != .filmstrip { browserMode = .filmstrip } else { viewer.toggleZoomAtCursor() }
        case .toggleFullScreen: viewer.state.fullScreen.toggle()
        case .togglePanels: panelsHidden.toggle()
        case .toggleFilmstrip: filmstripHidden.toggle()
        case .toggleStack:
            // First S turns burst stacking on (all collapsed); then S toggles the focused stack.
            if !browserState.stackBursts {
                browserState.stackBursts = true
                browserState.refresh(photos: photos)
            } else if let f = focusedID {
                browserState.stacks.toggleStack(of: f)
            }
            refocusIntoNavigation()
        case .copyAdjustments: copyWithOptions()
        case .applyAdjustments: applyToSelection()
        case .copyFromPrevious: copyFromPrevious()
        case .undo: undo()
        case .redo: redo()
        case .showCheatSheet: showCheatSheet.toggle()
        case .gridMode: browserMode = .grid
        case .viewerMode: browserMode = .filmstrip
        case .toggleBeforeAfter:
            // The grid shows no split: open the viewer with it on.
            if browserMode == .grid {
                guard focusedID != nil else { break } // nothing to compare: stay in the grid
                browserMode = .filmstrip
                if !viewer.state.beforeAfter { viewer.toggleBeforeAfter() }
            } else {
                viewer.toggleBeforeAfter()
            }
        case .togglePinCompare: viewer.togglePinCompare()
        case .brushSize(let step): stepBrushSize(step)
        case .tool(let t): // C crop, R straighten, I color picker
            if focusedPhoto?.isVideo == true { break }
            if browserMode != .filmstrip { browserMode = .filmstrip }
            viewer.toggleTool(t)
        case .maskTool(let t):
            if focusedPhoto == nil || focusedPhoto?.isVideo == true { break }
            if browserMode != .filmstrip { browserMode = .filmstrip }
            LayerController(app: self).chooseTool(t)
        case .whiteBalancePicker:
            if focusedPhoto == nil || focusedPhoto?.isVideo == true { break }
            if browserMode != .filmstrip { browserMode = .filmstrip }
            wbPickerActive.toggle()
        case .survey:
            if CommandCatalog.isEnabled(.survey, model: self) { _ = CommandCatalog.perform(.survey, model: self) }
        case .toggleClipping: viewer.clippingWarning.toggle()
        case .auto: Task { await autoAdjust() }
        case .toggleSoftProof:
            softProof.toggle()
            show(Toast(softProof.enabled ? softProof.statusText ?? "Soft proof on" : "Soft proof off", symbol: "printer"))
        case .selectAll: selectAll()
        case .escape:
            viewer.cancelCanvasDrag() // Esc abandons a drag in progress
            if optimizePanel != nil { endOptimize() }
            else if showCheatSheet { showCheatSheet = false }
            else if voice.helpShown { voice.helpShown = false }
            else if viewer.state.fullScreen { viewer.state.fullScreen = false }
            else if panelsHidden { panelsHidden = false }
            else if wbPickerActive { wbPickerActive = false } // before un-zooming: the loupe's picker
            else if focusPickerActive { focusPickerActive = false }
            else if colorTools.mode != nil { colorTools.cancel() }
            else if viewer.state.zoom == .actual { viewer.toggleZoomAtCursor() }
            else if viewer.canvasTool != .pan { viewer.canvasTool = .pan }
            else if let f = focusedID { focus(f, extend: false) }
        }
    }
}
