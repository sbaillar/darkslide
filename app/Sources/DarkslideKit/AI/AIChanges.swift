import Foundation

/// The texts of AI changes: a changed row's hover note and the AI Changes list's lines.
public enum AIChangeText {
    public static let listTitle = "AI Changes"
    public static let menuTitle = "AI Changes…"
    public static let undoAllTitle = "Undo All AI Changes"
    public static let undoOneTitle = "Undo AI Change"
    public static let empty = "No settings on this photo were changed by AI."
    /// A row whose earlier value isn't known (an AI Optimize report from an older version).
    public static let cantUndo = "Its value before AI isn't known, so it can't be undone here. Set it by hand, or use ⌘Z."

    /// Who changed it: "AI Optimize", "⌘K “warmer”", "Auto Adjust".
    public static func source(_ s: AIMarks.Source) -> String {
        switch s {
        case .optimize: "AI Optimize"
        case .command(let prompt): "⌘K “\(prompt)”"
        case .auto: "Auto Adjust"
        }
    }

    /// The source with its reason, after the dash of a hover note.
    static func because(_ c: AIChange) -> String {
        switch c.source {
        case .optimize: c.explanation.map { "AI Optimize: \($0)" } ?? "AI Optimize"
        case .command(let prompt): "⌘K: “\(prompt)”"
        case .auto: "Auto Adjust"
        }
    }

    /// A changed row's hover note: "AI changed it to +0.35 (was 0.00) — AI Optimize: …",
    /// or for a row without one value "AI added a sky layer — Auto Adjust". `now`: the
    /// photo's edit (a row the user changed is no longer an AI change, so it holds AI's value).
    public static func tooltip(_ field: AIField, _ c: AIChange, now: EditState) -> String {
        guard let e = AIFieldMap.entry(field) else { return "Changed by AI — \(because(c))" }
        guard let new = e.value(in: now) else { return "AI \(e.phrase(before: c.before, after: now)) — \(because(c))" }
        let was = c.before.flatMap(e.value(in:)).map { " (was \($0))" } ?? ""
        return "AI changed it to \(new)\(was) — \(because(c))"
    }

    /// The list's "old → new" ("0.00 → +0.35", "Off → On"), or what AI did ("Added a sky layer").
    public static func detail(_ field: AIField, _ c: AIChange, now: EditState) -> String {
        guard let e = AIFieldMap.entry(field) else { return "Changed" }
        guard let new = e.value(in: now) else { return capitalizedFirst(e.phrase(before: c.before, after: now)) }
        return "\(c.before.flatMap(e.value(in:)) ?? "?") → \(new)"
    }

    /// "just now", "5 min ago", "3 h ago", else the date; "earlier" when not known.
    public static func when(_ time: Date?, now: Date = Date()) -> String {
        guard let time else { return "earlier" }
        let s = now.timeIntervalSince(time)
        if s < 60 { return "just now" }
        if s < 3600 { return "\(Int(s / 60)) min ago" }
        if s < 86400 { return "\(Int(s / 3600)) h ago" }
        return time.formatted(date: .abbreviated, time: .shortened)
    }

    /// "a" or "an" before a word.
    static func article(_ word: String) -> String {
        "aeiou".contains(word.lowercased().first ?? "x") ? "an" : "a"
    }

    static func capitalizedFirst(_ s: String) -> String { s.prefix(1).uppercased() + s.dropFirst() }
}

/// One line of the AI Changes list.
public struct AIChangeRow: Identifiable, Equatable, Sendable {
    public var id: AIField { field }
    public let field: AIField
    /// The tool ("Exposure", "High Dynamic Range").
    public let tool: String
    /// The setting ("Exposure", "Red Hue", "Crop").
    public let setting: String
    public let detail: String
    public let source: String
    public let when: String
    public let tooltip: String
    /// Its pre-AI value is known: Undo puts it back.
    public let canUndo: Bool

    /// VoiceOver: one sentence for the line.
    public var accessibilityLabel: String { "\(tool), \(setting): \(detail), by \(source), \(when)" }
}

/// The JSON kept in a photo's sidecar (`ai_changes`): the AI changes grouped by the step
/// that made them, each step with the edit before it (ds-core adjustments JSON).
public enum AIChangesRecord {
    static let version = 1

    public static func encode(_ marks: AIMarks?) -> String {
        var steps: [(change: AIChange, rows: [[String: Any]])] = []
        let ordered = (marks?.changes ?? [:]).sorted { AIFieldMap.order($0.key) < AIFieldMap.order($1.key) }
        for (field, c) in ordered {
            var row: [String: Any] = ["panel": field.panel.rawValue, "name": field.name]
            if let x = c.explanation { row["explanation"] = x }
            var key = c
            key.explanation = nil
            if let i = steps.firstIndex(where: { $0.change == key }) { steps[i].rows.append(row) } else { steps.append((key, [row])) }
        }
        let json: [String: Any] = ["version": version, "steps": steps.map { s -> [String: Any] in
            var o: [String: Any] = ["rows": s.rows,
                                    "before": s.change.before.map { AdjustmentsCodec.overlay($0, onto: [:]) as Any } ?? NSNull()]
            switch s.change.source {
            case .optimize: o["source"] = "optimize"
            case .auto: o["source"] = "auto"
            case .command(let p):
                o["source"] = "command"
                o["prompt"] = p
            }
            if let t = s.change.time { o["time"] = t.timeIntervalSince1970 }
            return o
        }]
        let data = (try? JSONSerialization.data(withJSONObject: json, options: [.sortedKeys])) ?? Data()
        return String(decoding: data, as: UTF8.self)
    }

    /// The marks of a record (nil: none, or the record can't be read). Rows and sources
    /// this version doesn't know are skipped.
    public static func decode(_ json: String) -> AIMarks? {
        guard let obj = try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any],
              let steps = obj["steps"] as? [[String: Any]] else { return nil }
        var changes: [AIField: AIChange] = [:]
        for s in steps {
            let source: AIMarks.Source
            switch s["source"] as? String {
            case "optimize": source = .optimize
            case "auto": source = .auto
            case "command": source = .command(s["prompt"] as? String ?? "")
            default: continue
            }
            let time = (s["time"] as? Double).map(Date.init(timeIntervalSince1970:))
            let before = (s["before"] as? [String: Any]).map { AdjustmentsCodec.decode($0) }
            for r in s["rows"] as? [[String: Any]] ?? [] {
                guard let p = (r["panel"] as? String).flatMap(ToolPanelID.init(rawValue:)), let n = r["name"] as? String,
                      AIFieldMap.entry(AIField(p, n)) != nil else { continue }
                changes[AIField(p, n)] = AIChange(source: source, explanation: r["explanation"] as? String, time: time,
                                                  before: before)
            }
        }
        return changes.isEmpty ? nil : AIMarks(changes: changes)
    }
}

/// Stores that keep each photo's AI changes (in its sidecar), as `AIChangesRecord` JSON.
public protocol AIChangesStoring: Sendable {
    func aiChanges(for id: PhotoID) async throws -> String?
    func saveAIChanges(_ json: String?, for ids: [PhotoID]) async throws
}

extension AppModel {
    /// A photo's AI Changes list, in sidebar order.
    public func aiChangeRows(for id: PhotoID, now: Date = Date()) -> [AIChangeRow] {
        guard let marks = aiMarks[id] else { return [] }
        let edit = edit(for: id)
        return marks.changes.sorted { AIFieldMap.order($0.key) < AIFieldMap.order($1.key) }.map { field, c in
            AIChangeRow(field: field, tool: field.panel.title, setting: AIFieldMap.entry(field)?.label ?? field.name,
                        detail: AIChangeText.detail(field, c, now: edit), source: AIChangeText.source(c.source),
                        when: AIChangeText.when(c.time, now: now), tooltip: AIChangeText.tooltip(field, c, now: edit),
                        canUndo: c.before != nil)
        }
    }

    public var focusedAIChangeRows: [AIChangeRow] { focusedID.map { aiChangeRows(for: $0) } ?? [] }

    /// Undo the AI changes of these rows of the focused photo: each goes back to its value
    /// before AI changed it, as one undo step. Other rows (and the user's own edits since)
    /// stay; the rows leave the list.
    public func undoAIChanges(_ fields: Set<AIField>) {
        guard let f = focusedID else { return }
        revertAIChanges([f], only: fields)
    }

    /// Undo All AI Changes: the focused photo's, or with Edit All Selected each selected
    /// photo's own (one undo step). Photos not shown yet this run are read first.
    public func undoAllAIChanges() async {
        let ids = editAllSelected ? instructionTargets() : focusedID.map { [$0] } ?? []
        for id in ids { await loadAIChanges(for: id) }
        revertAIChanges(ids, only: nil)
    }

    public var canUndoAllAIChanges: Bool { undoAllAIChangesPhotoCount > 0 }

    /// The photos Undo All AI Changes would change (of those read so far).
    public var undoAllAIChangesPhotoCount: Int {
        let ids = editAllSelected ? instructionTargets() : focusedID.map { [$0] } ?? []
        return ids.filter { id in aiMarks[id]?.changes.values.contains { $0.before != nil } ?? false }.count
    }

    /// "Undo All AI Changes", or "… on 3 Photos" with Edit All Selected.
    public var undoAllAIChangesTitle: String {
        let n = undoAllAIChangesPhotoCount
        return n > 1 ? "\(AIChangeText.undoAllTitle) on \(n) Photos" : AIChangeText.undoAllTitle
    }

    /// Put rows (`nil`: all) back at their pre-AI values on each photo, as one undo step.
    private func revertAIChanges(_ ids: [PhotoID], only fields: Set<AIField>?) {
        var plan: [PhotoID: [(AIFieldMap.Entry, EditState)]] = [:]
        for id in ids {
            for (field, c) in aiMarks[id]?.changes ?? [:] where fields?.contains(field) ?? true {
                if let b = c.before, let e = AIFieldMap.entry(field) { plan[id, default: []].append((e, b)) }
            }
        }
        guard !plan.isEmpty else { return }
        let rows = plan.values.reduce(0) { $0 + $1.count }
        let label = rows == 1 ? "Revert AI Change to \(plan.values.first?.first?.0.label ?? "")" : "Revert \(rows) AI Changes"
        updateEdits(plan.keys.sorted { $0.raw < $1.raw }, label: label) { id, e in
            for (entry, before) in plan[id] ?? [] { entry.restore(&e, before) }
        }
        endEditGesture()
    }

    /// A row's Undo AI Change (its context menu) on the focused photo; nil when it has no
    /// AI change with a known earlier value.
    func aiUndo(_ field: AIField) -> (() -> Void)? {
        guard focusedAIMarks?.changes[field]?.before != nil else { return nil }
        return { [weak self] in self?.undoAIChanges([field]) }
    }

    /// Show or hide the AI Changes list.
    public func toggleAIChangesList() { aiChangesShown.toggle() }

    /// The rows' sidecar records, written in order with the reports.
    func saveAIChanges(_ marks: [PhotoID: AIMarks?]) {
        guard !marks.isEmpty, let store = sessions as? any AIChangesStoring else { return }
        let previous = reportSaves
        reportSaves = Task { [weak self] in
            await previous?.value
            for (id, m) in marks {
                do { try await store.saveAIChanges(AIChangesRecord.encode(m), for: [id]) } catch { self?.show(error) }
            }
        }
    }

    /// A photo's stored AI changes, the first time it is shown this run (a photo AI edited
    /// this run keeps its own set). Rows the edit no longer has are dropped. A photo
    /// without a record (never AI edited since changes were kept) falls back to its
    /// optimize report.
    func loadAIChanges(for id: PhotoID) async {
        guard !aiMarksSettled.contains(id), let store = sessions as? any AIChangesStoring else { return }
        await reportSaves?.value
        guard let json = try? await store.aiChanges(for: id), !aiMarksSettled.contains(id) else { return }
        applyMarks([id: AIChangesRecord.decode(json)?.pruned(against: edit(for: id))], save: false)
    }
}
