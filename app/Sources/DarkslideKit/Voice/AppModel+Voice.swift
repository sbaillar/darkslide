import Foundation

// MARK: - Voice commands

extension AppModel {
    /// Run spoken commands on the paths their keys and menu items use. Returns what
    /// the HUD echoes, or `nil` when nothing may run (a sheet owns the keyboard, as
    /// it does for keys).
    ///
    /// Chaining: "five next" rates, then advances. With auto-advance on, the rating
    /// already moved on, so the "next" right after it is not a second step, also
    /// when it arrives in a later call for the same utterance (`chain`).
    @discardableResult
    public func performVoice(_ actions: [VoiceAction], chain: inout VoiceChain) -> String? {
        guard sheet == nil, !commandBar.isPresented else { return nil }
        var ran: [String] = []
        for spoken in actions {
            var action = spoken
            if chain.advanced {
                // The rating already moved one photo: that was the first step of what follows.
                switch action {
                case .key(.move(.right, extend: false)):
                    chain.advanced = false
                    ran.append(action.label)
                    continue
                case .step(let n) where n > 1:
                    action = n == 2 ? .key(.move(.right, extend: false)) : .step(n - 1)
                case .batch(let b, let span) where !span.includesFocused:
                    action = .batch(b, VoiceSpan(count: span.count, includesFocused: true))
                default: break
                }
            }
            let before = focusedID
            guard let echo = runVoice(action) else { continue }
            ran.append(echo == action.label ? spoken.label : echo)
            chain.advanced = action.isCull && settings.autoAdvance && focusedID != before
        }
        return ran.isEmpty ? nil : ran.joined(separator: " · ")
    }

    /// One utterance on its own.
    @discardableResult
    public func performVoice(_ actions: [VoiceAction]) -> String? {
        var chain = VoiceChain()
        return performVoice(actions, chain: &chain)
    }

    /// One command; what the HUD echoes, or `nil` when it can't run now (its menu item
    /// would be disabled, nothing is there to move to).
    private func runVoice(_ action: VoiceAction) -> String? {
        switch action {
        case .key(let k):
            perform(k)
        case .menu(let m):
            guard CommandCatalog.isEnabled(m, model: self) else { return nil }
            CommandCatalog.perform(m, model: self)
        case .jump(let j):
            guard voiceJump(j) else { return nil }
        case .step(let n):
            guard voiceStep(n) else { return nil }
        case .goTo(let n):
            guard voiceGoTo(n) else { return nil }
        case .batch(let b, let span):
            return voiceBatch(b, span)
        case .repeated(let k, let n):
            for _ in 0..<n { perform(k) }
        case .showFlags(let f):
            filter.pick = f
        case .help(let shown):
            voice.helpShown = shown
        case .stopListening:
            voice.turnOff()
        case .setPanelsHidden(let hidden):
            if panelsHidden != hidden { perform(.togglePanels) }
        case .setFullScreen(let full):
            if viewer.state.fullScreen != full { perform(.toggleFullScreen) }
        case .edit(let text):
            // Opens Edit with Words with the phrase filled in: Return applies it (a
            // misheard instruction is never applied, nor sent anywhere, unasked).
            guard CommandCatalog.isEnabled(.askAI, model: self) else { return nil }
            commandBar.present()
            commandBar.text = text
        }
        return action.label
    }

    /// The list voice moves through: what the arrow keys walk.
    private var voiceNavigation: [PhotoID] { browserState.stackBursts ? navigationIDs : visibleIDs }

    /// First / last photo, or the next or previous matching one from the focused photo.
    private func voiceJump(_ jump: VoiceJump) -> Bool {
        let nav = voiceNavigation
        let target: PhotoID?
        switch jump {
        case .first: target = nav.first
        case .last: target = nav.last
        case .seek(let match, let forward):
            let current = navIndex(of: focusedID, in: nav)
            let wanted = { (id: PhotoID) in self.photo(id).map(match.matches) ?? false }
            target = forward ? nav.dropFirst(current.map { $0 + 1 } ?? 0).first(where: wanted)
                : nav.prefix(current ?? 0).last(where: wanted)
            if target == nil {
                show(Toast("No \(forward ? "more" : "earlier") \(match.adjective) photos", symbol: "mic"))
            }
        }
        guard let target else { return false }
        focus(target, extend: false)
        return true
    }

    /// "next 3" / "back 2": stops at the ends; false when it can't move at all.
    private func voiceStep(_ n: Int) -> Bool {
        let nav = voiceNavigation
        guard !nav.isEmpty else { return false }
        let current = navIndex(of: focusedID, in: nav)
        let target = min(max((current ?? (n > 0 ? -1 : nav.count)) + n, 0), nav.count - 1)
        guard target != current else {
            show(Toast(n > 0 ? "Already at the last photo" : "Already at the first photo", symbol: "mic"))
            return false
        }
        focus(nav[target], extend: false)
        return true
    }

    /// "photo 12": the 12th in the browser's order.
    private func voiceGoTo(_ n: Int) -> Bool {
        let nav = voiceNavigation
        guard n <= nav.count else {
            show(Toast("There is no photo \(n): \(nav.count) \(nav.count == 1 ? "is" : "are") shown", symbol: "mic"))
            return false
        }
        focus(nav[n - 1], extend: false)
        return true
    }

    /// One command on several photos, one undo step; says what it did ("Rejected 3 photos").
    /// Focus stays where it is, except that selecting selects exactly those photos and
    /// moves focus to the first of them.
    private func voiceBatch(_ batch: VoiceBatch, _ span: VoiceSpan) -> String? {
        let nav = voiceNavigation
        // Counted from the photo on screen: one a filter hides would start somewhere else.
        guard let focused = focusedID, visibleIndex(of: focused) != nil, let current = navIndex(of: focused, in: nav) else {
            show(Toast(focusedID == nil ? "No photo is focused" : "The focused photo is hidden by the filter", symbol: "mic"))
            return nil
        }
        let start = span.includesFocused ? current : current + 1
        let ids = Array(nav[min(start, nav.count)..<min(start + span.count, nav.count)])
        guard !ids.isEmpty else {
            show(Toast("No photos after this one", symbol: "mic"))
            return nil
        }
        let photos = "\(ids.count) photo\(ids.count == 1 ? "" : "s")"
        switch batch {
        case .rate(let r):
            updatePhotos(ids, label: r == 0 ? "Clear Rating of \(photos)" : "Rate \(photos) \(r)") { $0.rating = r }
            return r == 0 ? "Cleared the rating of \(photos)" : "Rated \(photos) \(String(repeating: "★", count: r))"
        case .pick, .reject, .unflag:
            let (state, verb, done): (PickState, String, String) = switch batch {
            case .pick: (.picked, "Pick", "Picked")
            case .reject: (.rejected, "Reject", "Rejected")
            default: (.none, "Unflag", "Unflagged")
            }
            updatePhotos(ids, label: "\(verb) \(photos)") { $0.pick = state }
            return "\(done) \(photos)"
        case .select:
            // Exactly the photos named, like "reject the next 3" acts on them, with focus on
            // the first: a plain "pick" then acts on what is on screen too.
            focus(ids[0], extend: false)
            selection = Set(ids)
            return "Selected \(photos)"
        case .apply:
            // Apply's own rules (`applyToSelection`): Copy's options, never onto the copied photo.
            guard let clipboard else {
                show(Toast("Copy adjustments first (“copy settings”)", symbol: "doc.on.clipboard"))
                return nil
            }
            let options = pasteOptions
            guard !options.groups.isEmpty else {
                show(Toast("No adjustments to apply: choose them in Copy’s options", symbol: "doc.on.clipboard"))
                return nil
            }
            let targets = ids.filter { $0 != clipboard.sourceID }
            guard !targets.isEmpty else {
                show(Toast("Apply never pastes onto the copied photo", symbol: "doc.on.clipboard"))
                return nil
            }
            let waiting = pendingPaste != nil // a second paste while one loads is dropped (and says so)
            pasteLoadingAsShotIfNeeded(to: targets, options: options)
            guard !waiting else { return nil }
            return "Applied adjustments to \(targets.count) photo\(targets.count == 1 ? "" : "s")"
        }
    }
}
