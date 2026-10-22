import Foundation

/// One control row of the tool sidebar an AI edit can change, keyed by its panel and a
/// field name — never by tab or position, since tabs and tool stacks are the user's to
/// arrange (a panel shows the same rows in whatever tab holds it).
public struct AIField: Hashable, Sendable {
    public let panel: ToolPanelID
    public let name: String

    public init(_ panel: ToolPanelID, _ name: String) {
        self.panel = panel
        self.name = name
    }
}

/// One row an AI edit changed: which AI, when, why, and the photo's edit before that
/// step (where the row's pre-AI values are read from to undo it).
public struct AIChange: Equatable, Sendable {
    public var source: AIMarks.Source
    /// AI Optimize's explanation of the change.
    public var explanation: String?
    /// When (`nil`: not known, a row read from an AI Optimize report stored before AI
    /// changes were kept).
    public var time: Date?
    /// The edit before the AI step that first changed the row (`nil`: not known, as for
    /// `time`; such a row can't be undone from the AI Changes list).
    public var before: EditState?

    public init(source: AIMarks.Source, explanation: String? = nil, time: Date? = nil, before: EditState? = nil) {
        self.source = source
        self.explanation = explanation
        self.time = time
        self.before = before
    }
}

/// The rows AI edits (AI Optimize, the ⌘K edit bar, Auto Adjust) changed on one photo.
/// The sidebar outlines them until the user changes that row, undoes it from the AI
/// Changes list, or clears them. A later AI step adds its rows (a row it changes again
/// keeps its first pre-AI values) and drops rows it put back.
public struct AIMarks: Equatable, Sendable {
    public enum Source: Equatable, Sendable {
        case optimize
        /// The ⌘K prompt.
        case command(String)
        case auto
    }

    public private(set) var changes: [AIField: AIChange]

    public init(changes: [AIField: AIChange]) {
        self.changes = changes
    }

    /// The rows an AI step changed from `before` to `after` (nil when none); an optimize
    /// step explains each row with the matching change of its `report`.
    public init?(source: Source, before: EditState, after: EditState, report: OptimizeReport? = nil, time: Date = Date()) {
        guard let m = Self.adding(to: nil, source: source, before: before, after: after, report: report, time: time) else {
            return nil
        }
        self = m
    }

    /// A stored optimize report's rows (a photo not edited by AI this session): the
    /// rows of its changes the edit still has (`stillApplied`, compared in Rust), or the
    /// first row of a change whose rows are all back at their defaults (a toggle turned
    /// back on). Their earlier values aren't known.
    public init?(report: OptimizeReport, stillApplied: Set<Int>, edit: EditState) {
        var changes: [AIField: AIChange] = [:]
        for c in report.appliedChanges where stillApplied.contains(c.id) {
            let rows = AIFieldMap.entries.filter { $0.optimize == c.field }
            let moved = rows.filter { $0.changed(EditState(), edit) }
            for e in moved.isEmpty ? Array(rows.prefix(1)) : moved {
                changes[e.field] = AIChange(source: .optimize, explanation: c.explanation)
            }
        }
        guard !changes.isEmpty else { return nil }
        self.changes = changes
    }

    /// `marks` after an AI step from `before` to `after`: its rows join (with their
    /// values in `before`, unless an earlier AI step already changed them), and rows now
    /// back at their pre-AI values leave.
    public static func adding(to marks: AIMarks?, source: Source, before: EditState, after: EditState,
                              report: OptimizeReport?, time: Date) -> AIMarks? {
        var changes = marks?.changes ?? [:]
        for f in AIFieldMap.changed(before, after) {
            let earlier = changes[f].map { $0.before } ?? before
            changes[f] = AIChange(source: source, explanation: explanation(f, source: source, report: report), time: time,
                                  before: earlier)
        }
        changes = changes.filter { f, c in
            guard let b = c.before, let e = AIFieldMap.entry(f) else { return true }
            return e.changed(b, after)
        }
        return changes.isEmpty ? nil : AIMarks(changes: changes)
    }

    public var fields: Set<AIField> { Set(changes.keys) }
    public var isEmpty: Bool { changes.isEmpty }
    /// The panels holding a changed row (header and tab badges).
    public var panels: Set<ToolPanelID> { Set(changes.keys.map(\.panel)) }

    /// The user changed (or undid) these rows: they are no longer AI changes.
    public func removing(_ fields: Set<AIField>) -> AIMarks? {
        var m = self
        for f in fields { m.changes[f] = nil }
        return m.isEmpty ? nil : m
    }

    /// The marks a stored record left, checked against the photo's edit now: a row whose
    /// values are back at their pre-AI values (changed outside this app) is dropped.
    public func pruned(against edit: EditState) -> AIMarks? {
        let kept = changes.filter { f, c in
            guard let b = c.before, let e = AIFieldMap.entry(f) else { return true }
            return e.changed(b, edit)
        }
        return kept.isEmpty ? nil : AIMarks(changes: kept)
    }

    static func explanation(_ field: AIField, source: Source, report: OptimizeReport?) -> String? {
        guard source == .optimize, let of = AIFieldMap.entry(field)?.optimize else { return nil }
        return report?.appliedChanges.first { $0.field == of }?.explanation
    }
}

/// A row's change across one undo step (`nil`: no highlights).
public struct MarksChange: Equatable, Sendable {
    public var before: AIMarks?
    public var after: AIMarks?

    public init(before: AIMarks?, after: AIMarks?) {
        self.before = before
        self.after = after
    }
}

/// Which edit values each sidebar row shows, how they read, how to put them back, and
/// which AI Optimize field sets them.
public enum AIFieldMap {
    public struct Entry: Sendable {
        public enum Kind: Sendable {
            /// A slider: its values read as the slider shows them.
            case slider(SliderSpec)
            /// An on/off row, named in a sentence ("turned AI Denoise on").
            case toggle(@Sendable (EditState) -> Bool, noun: String)
            /// A row without one value (curve, crop, layers), named in a sentence.
            case other(noun: String)
        }

        public let field: AIField
        /// The setting's name in the AI Changes list ("Exposure", "Red Hue", "Crop").
        public let label: String
        public let kind: Kind
        /// The AI Optimize field that sets this row.
        public let optimize: OptimizeField?
        /// A slider row's value (how `EditSlider` finds its row).
        let keyPath: (WritableKeyPath<EditState, Double> & Sendable)?
        let changed: @Sendable (EditState, EditState) -> Bool
        /// Put this row's values of `source` into an edit (undo the row's AI change).
        let restore: @Sendable (inout EditState, EditState) -> Void

        /// The row's value as its control shows it ("+0.35", "5,500 K", "On"); nil for
        /// rows without one value.
        public func value(in e: EditState) -> String? {
            switch kind {
            case .slider(let spec):
                if field.panel == .whiteBalance, e.whiteBalance.asShot { return "As Shot" }
                return keyPath.map { spec.string(e[keyPath: $0]) }
            case .toggle(let on, _): return on(e) ? "On" : "Off"
            case .other: return nil
            }
        }

        /// What an AI step did to the row, as a phrase after "AI": "changed the crop",
        /// "added a sky layer", "turned AI Denoise on".
        public func phrase(before: EditState?, after: EditState) -> String {
            switch kind {
            case .slider: return "changed \(label)"
            case .toggle(let on, let noun): return "turned \(noun) \(on(after) ? "on" : "off")"
            case .other(let noun):
                if field == AIFieldMap.layers, let b = before, after.layers.count > b.layers.count,
                   let added = after.layers.last(where: { l in !b.layers.contains { $0.id == l.id } }) {
                    return "added \(AIChangeText.article(added.name)) \(added.name.lowercased()) layer"
                }
                return "changed the \(noun)"
            }
        }
    }

    private static func slider(_ panel: ToolPanelID, _ name: String, _ spec: SliderSpec,
                               _ kp: WritableKeyPath<EditState, Double> & Sendable,
                               _ optimize: OptimizeField? = nil, label: String? = nil,
                               also: (@Sendable (EditState, EditState) -> Bool)? = nil,
                               restoreAlso: (@Sendable (inout EditState, EditState) -> Void)? = nil) -> Entry {
        Entry(field: AIField(panel, name), label: label ?? spec.label, kind: .slider(spec), optimize: optimize, keyPath: kp,
              changed: { a, b in a[keyPath: kp] != b[keyPath: kp] || (also?(a, b) ?? false) },
              restore: { e, s in
                  e[keyPath: kp] = s[keyPath: kp]
                  restoreAlso?(&e, s)
              })
    }

    private static func toggle(_ panel: ToolPanelID, _ name: String, _ label: String, noun: String,
                               _ kp: WritableKeyPath<EditState, Bool> & Sendable, _ optimize: OptimizeField?) -> Entry {
        Entry(field: AIField(panel, name), label: label, kind: .toggle({ $0[keyPath: kp] }, noun: noun), optimize: optimize,
              keyPath: nil, changed: { $0[keyPath: kp] != $1[keyPath: kp] }, restore: { $0[keyPath: kp] = $1[keyPath: kp] })
    }

    private static func row<V: Equatable>(_ panel: ToolPanelID, _ name: String, _ label: String, noun: String,
                                          _ kp: WritableKeyPath<EditState, V> & Sendable,
                                          _ optimize: OptimizeField? = nil) -> Entry {
        Entry(field: AIField(panel, name), label: label, kind: .other(noun: noun), optimize: optimize, keyPath: nil,
              changed: { $0[keyPath: kp] != $1[keyPath: kp] }, restore: { $0[keyPath: kp] = $1[keyPath: kp] })
    }

    // Rows that are not sliders.
    public static let levels = AIField(.levels, "levels")
    public static let curve = AIField(.curve, "curve")
    public static let colorBalance = AIField(.colorBalance, "wheels")
    public static let aiDenoise = AIField(.noise, "aiDenoise")
    public static let lensEnabled = AIField(.lensCorrection, "enabled")
    public static let chromaticAberration = AIField(.lensCorrection, "chromaticAberration")
    public static let cropFrame = AIField(.crop, "frame")
    public static let layers = AIField(.layers, "layers")

    /// In sidebar order (the AI Changes list's order).
    public static let entries: [Entry] = {
        // The Kelvin and tint sliders are display-only while As Shot: both rows carry it.
        let asShot: @Sendable (EditState, EditState) -> Bool = { $0.whiteBalance.asShot != $1.whiteBalance.asShot }
        let restoreAsShot: @Sendable (inout EditState, EditState) -> Void = { $0.whiteBalance.asShot = $1.whiteBalance.asShot }
        var e: [Entry] = [
            slider(.whiteBalance, "temperature", Specs.temperature, \.whiteBalance.temperature, .whiteBalance,
                   also: asShot, restoreAlso: restoreAsShot),
            slider(.whiteBalance, "tint", Specs.tint, \.whiteBalance.tint, .whiteBalance, also: asShot,
                   restoreAlso: restoreAsShot),
            slider(.exposure, "ev", Specs.exposure, \.exposure.ev, .exposure),
            slider(.exposure, "contrast", Specs.contrast, \.exposure.contrast, .contrast),
            slider(.exposure, "brightness", Specs.brightness, \.exposure.brightness),
            slider(.exposure, "saturation", Specs.saturation, \.exposure.saturation, .saturation),
            slider(.presence, "vibrance", LayerToolSpecs.vibrance, \.presence.vibrance, .vibrance),
            slider(.presence, "texture", LayerToolSpecs.texture, \.presence.texture, .texture),
            slider(.presence, "dehaze", LayerToolSpecs.dehaze, \.presence.dehaze, .dehaze),
            slider(.hdr, "highlights", Specs.highlights, \.hdr.highlights, .highlights),
            slider(.hdr, "shadows", Specs.shadows, \.hdr.shadows, .shadows),
            slider(.hdr, "whites", Specs.whites, \.hdr.whites),
            slider(.hdr, "blacks", Specs.blacks, \.hdr.blacks),
            row(.levels, "levels", "Levels", noun: "levels", \.levels, .levels),
            row(.curve, "curve", "Curve", noun: "curve", \.curve),
            row(.colorBalance, "wheels", "Color Wheels", noun: "color balance", \.colorBalance),
        ]
        for (i, band) in ColorEditorEdit.bandNames.enumerated() {
            e += [
                slider(.colorEditor, "band\(i).hue", Specs.bandHue, \.colorEditor.bands[i].hueShift, label: "\(band) Hue"),
                slider(.colorEditor, "band\(i).saturation", Specs.bandSaturation, \.colorEditor.bands[i].saturation,
                       label: "\(band) Saturation"),
                slider(.colorEditor, "band\(i).lightness", Specs.bandLightness, \.colorEditor.bands[i].lightness,
                       label: "\(band) Lightness"),
            ]
        }
        e += [
            slider(.clarity, "clarity", Specs.clarity, \.detail.clarity, .clarity),
            slider(.clarity, "structure", Specs.structure, \.detail.structure),
            slider(.sharpening, "amount", Specs.sharpenAmount, \.detail.sharpenAmount),
            slider(.sharpening, "radius", Specs.sharpenRadius, \.detail.sharpenRadius),
            slider(.sharpening, "threshold", Specs.sharpenThreshold, \.detail.sharpenThreshold),
            toggle(.noise, "aiDenoise", "AI Denoise", noun: "AI Denoise", \.noise.aiDenoise, .aiDenoise),
            slider(.noise, "aiStrength", Specs.aiDenoiseStrength, \.noise.aiStrength, .aiDenoise),
            slider(.noise, "luminance", Specs.noiseLuminance, \.noise.luminance, .noiseReduction),
            slider(.noise, "color", Specs.noiseColor, \.noise.color, .noiseReduction),
            slider(.noise, "detail", Specs.noiseDetail, \.noise.detail),
            slider(.vignette, "amount", Specs.vignetteAmount, \.vignette.amountEV),
            slider(.vignette, "midpoint", Specs.vignetteMidpoint, \.vignette.midpoint),
            toggle(.lensCorrection, "enabled", "Lens Profile", noun: "lens correction", \.lens.enabled, .lensCorrection),
            slider(.lensCorrection, "distortion", Specs.distortion, \.lens.distortion, .lensCorrection),
            slider(.lensCorrection, "lightFalloff", Specs.lightFalloff, \.lens.lightFalloff, .lensCorrection),
            toggle(.lensCorrection, "chromaticAberration", "Chromatic Aberration", noun: "chromatic aberration removal",
                   \.lens.chromaticAberration, .lensCorrection),
            slider(.lensCorrection, "keystoneVertical", Specs.keystoneVertical, \.lens.keystoneVertical),
            slider(.lensCorrection, "keystoneHorizontal", Specs.keystoneHorizontal, \.lens.keystoneHorizontal),
            slider(.crop, "angle", Specs.straighten, \.crop.angle, .straighten),
            Entry(field: cropFrame, label: "Crop", kind: .other(noun: "crop"), optimize: .straighten, keyPath: nil,
                  changed: { a, b in a.crop.aspect != b.crop.aspect || a.crop.rect != b.crop.rect
                      || a.crop.quarterTurns != b.crop.quarterTurns },
                  restore: { e, s in
                      e.crop.aspect = s.crop.aspect
                      e.crop.rect = s.crop.rect
                      e.crop.quarterTurns = s.crop.quarterTurns
                  }),
            row(.layers, "layers", "Layers", noun: "layers", \.layers, .skyLayer),
        ]
        return e
    }()

    private static let byField: [AIField: Int] = Dictionary(uniqueKeysWithValues: entries.enumerated().map { ($1.field, $0) })

    public static func entry(_ field: AIField) -> Entry? { byField[field].map { entries[$0] } }

    /// A row's place in the sidebar order.
    static func order(_ field: AIField) -> Int { byField[field] ?? entries.count }

    /// The rows whose values differ between two edits.
    public static func changed(_ a: EditState, _ b: EditState) -> Set<AIField> {
        guard a != b else { return [] }
        return Set(entries.filter { $0.changed(a, b) }.map(\.field))
    }

    /// The row of a slider bound to `keyPath`.
    public static func field(for keyPath: WritableKeyPath<EditState, Double>) -> AIField? {
        let kp: AnyKeyPath = keyPath
        return entries.first { $0.keyPath.map { $0 as AnyKeyPath } == kp }?.field
    }
}

/// Words and colours of the highlights (kept here so they are tested).
public enum AIHighlightStyle {
    /// The row tint: the AI yellow at this opacity over the panel, under its outline.
    public static let tintOpacity = 0.06
    /// The outline around a changed row.
    public static let borderWidth = 1.5
    public static let badgeSymbol = "sparkles"
    public static let panelBadgeHelp = "Contains settings changed by AI (click to list them)"
    public static let clearTitle = "Clear AI Highlights"
    public static let showTitle = "Show AI-Changed Tools"
    /// VoiceOver, after a panel header's or tool tab's name.
    public static let containerSuffix = ", has settings changed by AI"

    /// VoiceOver: the control's value, then that AI changed it.
    public static func accessibilityValue(_ value: String) -> String {
        value.isEmpty ? "changed by AI" : "\(value), changed by AI"
    }

    /// The tinted row background: `color` over the panel grey at `tintOpacity`.
    public static func tinted(_ color: RGBColor, onGrey grey: Double) -> RGBColor {
        let t = tintOpacity
        return RGBColor(grey + (color.r - grey) * t, grey + (color.g - grey) * t, grey + (color.b - grey) * t)
    }

    /// Whether a tab holding `panels` has a changed row.
    public static func tabHasChanges(_ panels: [ToolPanelID], changed: Set<ToolPanelID>) -> Bool {
        panels.contains(where: changed.contains)
    }
}

extension AppModel {
    /// The focused photo's AI-changed rows.
    public var focusedAIMarks: AIMarks? { focusedID.flatMap { aiMarks[$0] } }

    /// The hover text of a changed row of the focused photo (nil: not changed by AI):
    /// "AI changed it to +0.35 (was 0.00) — AI Optimize: …".
    public func aiNote(_ field: AIField) -> String? {
        guard let f = focusedID, let c = aiMarks[f]?.changes[field] else { return nil }
        return AIChangeText.tooltip(field, c, now: edit(for: f))
    }

    /// Panels with a changed row on the focused photo.
    public var aiChangedPanels: Set<ToolPanelID> { focusedAIMarks?.panels ?? [] }

    /// Set (or with `nil` clear) photos' highlights, and keep them in the sidecars
    /// (`save: false`: they were just read from there, or come from a stored report).
    func applyMarks(_ marks: [PhotoID: AIMarks?], save: Bool = true) {
        for (id, m) in marks {
            aiMarks[id] = m
            aiMarksSettled.insert(id)
        }
        if save { saveAIChanges(marks) }
    }

    /// The highlights a step leaves: an AI step's rows join the photo's set; any other
    /// step clears the rows it changed. Only photos whose set changes are listed.
    func marksChanges(_ step: HistoryStep, ai: AIMarks.Source?) -> [PhotoID: MarksChange] {
        var out: [PhotoID: MarksChange] = [:]
        for (id, after) in step.editsAfter {
            guard let before = step.editsBefore[id] else { continue }
            let old = aiMarks[id]
            let new: AIMarks? = if let ai {
                AIMarks.adding(to: old, source: ai, before: before, after: after, report: step.reports[id]?.after,
                               time: step.time)
            } else {
                old?.removing(AIFieldMap.changed(before, after))
            }
            if new != old { out[id] = MarksChange(before: old, after: new) }
        }
        return out
    }

    /// Clear the highlights of the photos an AI edit would target (the focused photo, or
    /// the selection it belongs to). Not an undo step.
    public func clearAIHighlights() {
        applyMarks(Dictionary(uniqueKeysWithValues: instructionTargets().filter { aiMarks[$0] != nil }.map { ($0, nil) }))
    }

    public var canClearAIHighlights: Bool { instructionTargets().contains { aiMarks[$0] != nil } }

    /// A photo's highlights from its stored optimize report, the first time the report
    /// is loaded this session (a photo AI edited this session, or with stored AI changes,
    /// keeps its own set).
    func deriveAIMarks(_ id: PhotoID, from report: OptimizeReport) {
        guard !aiMarksSettled.contains(id) else { return }
        let edit = edit(for: id)
        let still = (try? optimizer?.stillApplied(report, in: edit)) ?? Set(report.appliedChanges.map(\.id))
        applyMarks([id: AIMarks(report: report, stillApplied: still, edit: edit)], save: false)
    }
}
