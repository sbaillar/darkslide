import LuxtableAI
import Foundation

/// An AI service that can turn text into a typed `EditIntent` (so the app can apply
/// it, run Auto Straighten for "straighten", and say what it didn't understand).
public protocol InstructionInterpreting: Sendable {
    func editIntent(for text: String) async throws -> EditIntent
}

extension VisionAIService: InstructionInterpreting {
    public func editIntent(for text: String) async throws -> EditIntent { try await language.intent(for: text) }
}

/// What an instruction changed, in words ("+1.0 EV, 5235→6435 K, +20 contrast").
public enum InstructionSummary {
    /// "+0.25 EV", "+1.0 EV", "-0.5 EV": two decimals only when a quarter stop needs them.
    static func ev(_ d: Double) -> String {
        let hundredths = (d * 100).rounded()
        return String(format: Int(hundredths) % 10 == 0 ? "%+.1f EV" : "%+.2f EV", hundredths / 100)
    }

    public static func describe(before b: EditState, after a: EditState, asShot: WhiteBalanceValue?) -> String {
        var parts: [String] = []
        if a.whiteBalance != b.whiteBalance {
            let from = b.whiteBalance.asShot ? (asShot?.kelvin ?? b.whiteBalance.temperature) : b.whiteBalance.temperature
            let fromTint = b.whiteBalance.asShot ? (asShot?.tint ?? b.whiteBalance.tint) : b.whiteBalance.tint
            if a.whiteBalance.temperature.rounded() != from.rounded() {
                parts.append("\(Safe.clampedInt(from))→\(Safe.clampedInt(a.whiteBalance.temperature)) K")
            }
            if abs(a.whiteBalance.tint - fromTint) >= 0.05 { parts.append(signed(a.whiteBalance.tint - fromTint, "tint")) }
        }
        if a.exposure.ev != b.exposure.ev { parts.append(InstructionSummary.ev(a.exposure.ev - b.exposure.ev)) }
        if a.exposure.contrast != b.exposure.contrast { parts.append(signed(a.exposure.contrast - b.exposure.contrast, "contrast")) }
        if a.exposure.saturation != b.exposure.saturation {
            parts.append(a.exposure.saturation <= -100 ? "black and white"
                         : signed(a.exposure.saturation - b.exposure.saturation, "saturation"))
        }
        if a.hdr.highlights != b.hdr.highlights { parts.append(signed(a.hdr.highlights - b.hdr.highlights, "highlights")) }
        if a.hdr.shadows != b.hdr.shadows { parts.append(signed(a.hdr.shadows - b.hdr.shadows, "shadows")) }
        if a.colorEditor != b.colorEditor { parts.append("colour bands") }
        if a.crop.aspect != b.crop.aspect || a.crop.rect != b.crop.rect { parts.append("crop \(a.crop.aspect.rawValue)") }
        if a.crop.angle != b.crop.angle { parts.append(String(format: "straighten %+.1f°", a.crop.angle - b.crop.angle)) }
        return parts.joined(separator: ", ")
    }

    static func signed(_ v: Double, _ name: String) -> String {
        let r = (v * 10).rounded() / 10
        return (r >= 0 ? "+" : "−") + (r == r.rounded() ? String(Int(abs(r))) : String(format: "%.1f", abs(r))) + " " + name
    }
}

/// Words that drive history instead of an edit.
public enum InstructionCommand: Equatable, Sendable {
    case undo, redo, reset

    public init?(_ text: String) {
        let t = text.lowercased().trimmingCharacters(in: .whitespacesAndNewlines.union(.punctuationCharacters))
        switch t {
        case "undo", "undo that", "undo it", "undo the last edit", "undo last", "undo the last change", "undo please",
             "go back":
            self = .undo
        case "redo", "redo that": self = .redo
        case "reset", "reset all", "reset everything", "reset edits", "reset the edits", "start over", "revert",
             "revert to original", "back to original":
            self = .reset
        default: return nil
        }
    }
}

extension AppModel {
    /// Apply a typed instruction (⌘K command bar) to the focused photo, or to
    /// every selected photo when several are selected: "undo"/"redo"/"reset" drive
    /// history; otherwise the guarded parser's intent (tone, crop aspect and "straighten",
    /// which runs Auto Straighten per photo) is applied as ONE undo step, and the toast
    /// says what changed, on how many photos, and what wasn't understood or levelled.
    /// Everything lands on the photos targeted when the command was submitted, even if
    /// focus moves while the model answers.
    /// Returns the toast it showed (nil when there was nothing to do).
    @discardableResult
    public func runInstruction(_ text: String) async -> Toast? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        if let command = InstructionCommand(trimmed) {
            switch command {
            case .undo: undo()
            case .redo: redo()
            case .reset:
                let ids = instructionTargets().filter { edit(for: $0) != EditState() }
                guard !ids.isEmpty else { return nil }
                updateEdits(ids, label: "Reset") { _, e in e = EditState() }
                endEditGesture()
                show(Toast("Reset \(ids.count == 1 ? "" : "\(ids.count) photos ")to the original. ⌘Z to undo",
                           symbol: "arrow.counterclockwise"))
            }
            return toast
        }
        let ids = instructionTargets()
        guard !ids.isEmpty else { return nil }
        let session = currentSession?.id
        guard let interpreter = ai as? any InstructionInterpreting else {
            await applyInstructionViaService(trimmed, ids: ids, session: session)
            return toast
        }
        do {
            let intent = try await interpreter.editIntent(for: trimmed)
            await loadDevelopInfo(for: ids) // "warmer" on As Shot starts from the real as-shot K/tint
            guard isStill(in: session) else { return nil }
            let bases = Dictionary(ids.map { ($0, edit(for: $0)) }) { a, _ in a }
            var edits = intent
            edits.straighten = false
            edits.unrecognized = []
            // Never a number of degrees and Auto Straighten together: typed degrees win.
            let level = intent.straighten && intent.straightenDegrees == nil
            // An aspect ("crop 4:5") crops like the aspect chips do (the largest centred
            // box of that ratio in the current crop), not just records the ratio.
            let aspect = edits.cropAspect.flatMap(CropPreset.init(rawValue:))
            if aspect != nil { edits.cropAspect = nil }
            var angles: [PhotoID: Double] = [:]
            if level {
                for id in ids {
                    // A detector error counts as "no clear horizon" for that photo.
                    if let p = photo(id), let a = try? await horizonAngle(for: p) { angles[id] = a }
                }
            }
            guard isStill(in: session) else { return nil } // another session opened meanwhile
            // Tone, crop and straighten land as one undo step on every target.
            updateEdits(ids, label: "“\(trimmed)”", ai: .command(trimmed)) { id, e in
                if !edits.isEmpty, let base = bases[id] {
                    e = AIEditMerge.apply(result: base.applying(edits, asShot: asShot(for: id)), base: base, current: e)
                }
                if let aspect { crop(&e, to: aspect, photo: id) }
                if let a = angles[id] { e.crop.angle = a }
            }
            endEditGesture()
            let changed = ids.filter { edit(for: $0) != bases[$0] }
            // Say what changed on the first changed photo (the focused one when it changed).
            let summary = changed.first.map {
                InstructionSummary.describe(before: bases[$0] ?? EditState(), after: edit(for: $0), asShot: asShot(for: $0))
            } ?? ""
            let t = Self.instructionToast(summary: summary, photos: changed.count, targets: ids.count,
                                          unlevelled: level ? ids.count - angles.count : 0,
                                          unrecognized: intent.unrecognized)
            show(t)
            return t
        } catch is CancellationError {
            return nil
        } catch {
            show(error)
            return toast
        }
    }

    /// The photos an instruction edits: the selection when several are selected and the
    /// focused photo is one of them, else the focused photo (never videos).
    func instructionTargets() -> [PhotoID] {
        guard let f = focusedID else { return [] }
        let ids = selectedIDs.count > 1 && selectedIDs.contains(f) ? selectedIDs : [f]
        return ids.filter { photo($0)?.isVideo != true }
    }

    /// The service-level fallback for AI services that don't expose intents.
    private func applyInstructionViaService(_ text: String, ids: [PhotoID], session: UUID?) async {
        do {
            await loadDevelopInfo(for: ids)
            var results: [PhotoID: (base: EditState, result: EditState)] = [:]
            for id in ids {
                let base = edit(for: id)
                results[id] = (base, try await ai.applyInstruction(text, to: base, asShot: asShot(for: id)))
            }
            guard isStill(in: session) else { return }
            updateEdits(ids, label: "“\(text)”", ai: .command(text)) { id, e in
                if let r = results[id] { e = AIEditMerge.apply(result: r.result, base: r.base, current: e) }
            }
            endEditGesture()
            let changed = ids.filter { edit(for: $0) != results[$0]?.base }
            let summary = changed.first.map {
                InstructionSummary.describe(before: results[$0]?.base ?? EditState(), after: edit(for: $0), asShot: asShot(for: $0))
            } ?? ""
            show(Self.instructionToast(summary: summary, photos: changed.count, targets: ids.count, unlevelled: 0,
                                       unrecognized: []))
        } catch {
            show(error)
        }
    }

    /// `photos`: how many changed (named when more than one); `unlevelled`: how many of
    /// the `targets` "straighten" found no horizon for (never dropped silently).
    static func instructionToast(summary: String, photos: Int = 1, targets: Int = 1, unlevelled: Int = 0,
                                 unrecognized: [String]) -> Toast {
        var parts: [String] = []
        if !summary.isEmpty { parts.append(photos > 1 ? "\(summary) on \(photos) photos" : summary) }
        let unknown = unrecognized.isEmpty ? nil : "Didn't understand: " + unrecognized.map { "“\($0)”" }.joined(separator: ", ")
        if parts.isEmpty {
            if let unknown { return Toast(unknown, symbol: "questionmark.bubble", isError: true) }
            return Toast(unlevelled > 0 ? "No clear horizon found; nothing was straightened" : "Nothing to change",
                         symbol: "sparkles")
        }
        if unlevelled > 0 {
            parts.append(targets > 1 ? "no clear horizon on \(unlevelled) — not straightened"
                                     : "no clear horizon — not straightened")
        }
        var message = parts.joined(separator: ", ") + ". ⌘Z to undo"
        if let unknown { message += ". " + unknown }
        return Toast(message, symbol: "sparkles")
    }
}
