import DarkslideAI
import Foundation

/// The controls an instruction can move (what the over-edit guard masks by).
public enum EditControl: String, CaseIterable, Sendable {
    case whiteBalance, tint, exposure, contrast, highlights, shadows, saturation, crop, straighten, color
}

/// Which controls a clause of text *mentions*, and how strongly.
///
/// A model backend (Foundation Models, Jev) may only move controls the user's
/// words name, by at most the "a bit / — / a lot" step the words ask for, or
/// exactly the number they give. Deterministic, so it's unit-tested phrase by phrase.
public enum ControlMentions {
    /// Whole words only (M1): a stem matches at a word start with just the endings
    /// listed, so "ev" isn't in "never"/"evening", "light" isn't in "slightly".
    static let vocabulary: [(EditControl, [String])] = [
        (.whiteBalance, ["warm(er|est|th|ing|ed)?", "cool(er|est|ing|ed)?", "cold(er)?", "temp(erature)?", "kelvin",
                         "white balance", "wb", "yellow(er|ish|y)?", "orange(y|ish)?", "blue(r|ish|y)?", "daylight",
                         "tungsten", "cloudy", "shade", #"\d+ ?k"#]),
        (.tint, ["tint(ed|s)?", "magenta(s)?", "green(er|ish|s)?"]),
        (.exposure, ["bright(er|est|en|ens|ened|ening|ness)?", "dark(er|est|en|ens|ened|ening|ness)?",
                     "(over|under)?expos(e|ed|ure)", "stops?", "evs?", "light(er|en|ens|ened|ening)?", "dim(mer|med)?",
                     "dull(er)?"]),
        (.contrast, ["contrast(y)?", "punch(y|ier)?", "flat(ter)?", "pop(s|py)?"]),
        (.highlights, ["highlights?", "sky", "skies", "blown", "bright areas"]),
        (.shadows, ["shadows?", "dark areas", "blacks"]),
        (.saturation, ["(de|over|under)?saturat(e|ed|ion)", "vivid", "vibran(t|ce|cy)", "colou?r(s|ful|ed)?", "muted",
                       "black (and|&) white", "b&w", "mono(chrome)?", "gr[ae]yscale"]),
        (.crop, ["crop(ped|ping|s)?", "aspect", "square"]),
        (.straighten, ["straight(en|er)?", "level(led|ed)?", "horizon(tal)?", "tilt(ed|s)?", "crooked", "askew",
                       "rotat(e|ed|ion)", "degrees?", #"\d+(\.\d+)?\s*°"#]),
    ]
    /// Of the straighten words, those that ask for a level horizon (Auto Straighten).
    static let levelWords = ["straight(en|er)?", "level(led|ed)?", "horizon(tal)?", "tilt(ed|s)?", "crooked", "askew"]
    static let smallWords = ["a bit", "slight(ly)?", "a little", "a touch", "a tad", "subtl(e|y)", "somewhat", "hint",
                             "gently", "mild(ly)?"]
    static let bigWords = ["a lot", "much", "lots", "way", "very", "strong(ly|er)?", "really", "significantly"]
    /// About the conversation, not the photo: never an edit.
    static let metaWords = ["never ?mind", "nevermind", "forget (it|that|about it)", "ignore", "disregard", "cancel",
                            "undo", "redo", "instructions?", "prompt"]

    private static func words(_ alternatives: [String]) -> NSRegularExpression {
        // Not a letter or digit on either side (so "b&w" and "5500k" match too).
        let pattern = #"(?<![\p{L}\p{N}])(?:"# + alternatives.joined(separator: "|") + #")(?![\p{L}\p{N}])"#
        return try! NSRegularExpression(pattern: pattern, options: [.caseInsensitive]) // literal patterns above
    }

    private static let patterns = vocabulary.map { ($0.0, words($0.1)) }
    private static let level = words(levelWords)
    private static let small = words(smallWords)
    private static let big = words(bigWords)
    private static let meta = words(metaWords)

    private static func matches(_ re: NSRegularExpression, _ s: String) -> Bool {
        re.firstMatch(in: s, range: NSRange(s.startIndex..., in: s)) != nil
    }

    /// Controls named in `clause`, word by word.
    public static func controls(in clause: String) -> Set<EditControl> {
        Set(patterns.filter { matches($0.1, clause) }.map(\.0))
    }

    /// The clause asks for a level horizon ("fix the tilt", "straighten"), not a number of degrees.
    public static func asksToLevel(_ clause: String) -> Bool { matches(level, clause) }

    /// "never mind", "ignore previous instructions", "undo that": not an edit.
    public static func isMeta(_ clause: String) -> Bool { matches(meta, clause) }

    /// Intensity multiplier the words ask for: "a bit" 0.5, none 1, "much/a lot" 2.
    public static func intensity(of clause: String) -> Double {
        if matches(small, clause) { return 0.5 }
        if matches(big, clause) { return 2 }
        return 1
    }

    /// The first plain number in `clause` ("warmer by 400" → 400, "7 stops" → 7);
    /// ordinals ("the 2nd shot") aren't amounts.
    public static func number(in clause: String) -> Double? {
        let scanner = clause.replacingOccurrences(of: ",", with: ".")
        let pattern = #"(?<![\d.])[-+]?\d+(\.\d+)?(?!\d|\.\d|(st|nd|rd|th)\b)"#
        guard let r = scanner.range(of: pattern, options: .regularExpression) else { return nil }
        return Double(scanner[r])
    }

    /// Degrees the user typed ("rotate 2 degrees", "-3°"), signed as written.
    public static func degrees(in clause: String) -> Double? {
        let scanner = clause.replacingOccurrences(of: ",", with: ".")
        let pattern = #"[-+]?\d+(\.\d+)?(?=\s*(°|deg(ree)?s?\b))"#
        guard let r = scanner.range(of: pattern, options: [.regularExpression, .caseInsensitive]) else { return nil }
        return Double(scanner[r])
    }
}

/// Masks and clamps a model's `EditIntent` to what the text asked for.
public enum IntentGuard {
    /// Default step per control when the words give no number (ds-ai's rules steps;
    /// white balance in Kelvin).
    static let step: [EditControl: Double] = [
        .whiteBalance: 500, .tint: 10, .exposure: 0.5, .contrast: 20, .highlights: 25, .shadows: 25, .saturation: 20,
    ]
    /// Largest value a number may ask for (beyond it the clause isn't applied).
    static let range: [EditControl: Double] = [
        .whiteBalance: 3000, .tint: 50, .exposure: 3, .contrast: 100, .highlights: 100, .shadows: 100, .saturation: 100,
        .straighten: 45,
    ]

    public struct Outcome: Equatable, Sendable {
        public var intent: EditIntent
        /// Clauses nothing was applied for (with a reason when it's a range).
        public var unrecognized: [String]
    }

    /// Keep only the controls each clause mentions, at the magnitude its words ask for.
    public static func constrain(_ model: EditIntent, clauses: [String]) -> Outcome {
        var out = EditIntent()
        var unknown: [String] = []
        for clause in clauses {
            // "never mind", "ignore previous instructions": reported, never an edit (M1).
            if ControlMentions.isMeta(clause) {
                unknown.append(clause)
                continue
            }
            let mentioned = ControlMentions.controls(in: clause)
            let scale = ControlMentions.intensity(of: clause)
            let number = ControlMentions.number(in: clause)
            var applied = false
            func take(_ c: EditControl, _ v: Double, _ set: (inout EditIntent, Double) -> Void) {
                guard mentioned.contains(c), v != 0, v.isFinite else { return }
                var magnitude: Double
                if let n = number.map(abs) {
                    guard n <= (range[c] ?? .infinity) else { return }
                    magnitude = n
                } else {
                    magnitude = min(abs(v), (step[c] ?? abs(v)) * scale)
                }
                set(&out, v < 0 ? -magnitude : magnitude)
                applied = true
            }
            take(.whiteBalance, model.temperatureShift) { $0.temperatureShift = $1 }
            take(.tint, model.tintShift) { $0.tintShift = $1 }
            take(.exposure, model.exposureShift) { $0.exposureShift = $1 }
            take(.contrast, model.contrastShift) { $0.contrastShift = $1 }
            take(.highlights, model.highlightsShift) { $0.highlightsShift = $1 }
            take(.shadows, model.shadowsShift) { $0.shadowsShift = $1 }
            take(.saturation, model.saturationShift) { $0.saturationShift = $1 }
            // H3: a rotation is never the model's number. Degrees only as the user typed
            // them ("rotate 2 degrees", "-3°"; the model may only pick the turn's direction
            // when the words give no sign); "fix the tilt" / "level" is Auto Straighten
            // (Vision's horizon); never both.
            let asksRotation = model.straighten || model.straightenDegrees != nil
            if mentioned.contains(.straighten), asksRotation {
                if let typed = ControlMentions.degrees(in: clause) {
                    if abs(typed) <= range[.straighten]! {
                        let explicitSign = clause.range(of: #"[-+]\d"#, options: .regularExpression) != nil
                        let sign: Double = explicitSign || (model.straightenDegrees ?? 1) >= 0 ? 1 : -1
                        out.straightenDegrees = sign * typed
                        applied = true
                    }
                } else if ControlMentions.asksToLevel(clause) {
                    out.straighten = true
                    applied = true
                }
            }
            if mentioned.contains(.crop), let a = model.cropAspect { out.cropAspect = a; applied = true }
            if !applied {
                let n = mentioned.contains(.straighten) ? ControlMentions.degrees(in: clause) ?? number : number
                if let n, let c = EditControl.allCases.first(where: { mentioned.contains($0) && range[$0] != nil }),
                   abs(n) > range[c]! {
                    unknown.append("\(clause) (beyond the ±\(String(format: "%g", range[c]!)) range)")
                } else {
                    unknown.append(clause)
                }
            }
        }
        if out.straightenDegrees != nil { out.straighten = false } // the typed number wins
        return Outcome(intent: out, unrecognized: unknown)
    }

    /// Rules win: the model only fills controls the rules left untouched.
    public static func merge(rules r: EditIntent, model m: EditIntent) -> EditIntent {
        var out = r
        let wb = r.temperatureShift != 0 || r.warmth != 0 || r.temperatureTarget != nil
        if !wb { out.temperatureShift = m.temperatureShift }
        if r.tintShift == 0 { out.tintShift = m.tintShift }
        if r.exposureShift == 0 { out.exposureShift = m.exposureShift }
        if r.contrastShift == 0 { out.contrastShift = m.contrastShift }
        if r.highlightsShift == 0 { out.highlightsShift = m.highlightsShift }
        if r.shadowsShift == 0 { out.shadowsShift = m.shadowsShift }
        if r.saturationShift == 0 { out.saturationShift = m.saturationShift }
        if r.cropAspect == nil { out.cropAspect = m.cropAspect }
        if r.straightenDegrees == nil, !r.straighten {
            out.straightenDegrees = m.straightenDegrees
            out.straighten = m.straighten
        }
        return out
    }
}

/// Natural-language edits with an over-edit guard: ds-ai's rule parser runs
/// first; only the clauses it didn't understand go to the model backends (Jev,
/// then Foundation Models, in order; the first that answers is used), whose
/// answer is masked to the controls those clauses name and clamped to the step
/// their words ask for. What nobody understood comes back in `unrecognized`.
public struct GuardedEditService: NaturalLanguageEditService {
    public let rules: any NaturalLanguageEditService
    public let models: [any NaturalLanguageEditService]
    /// Told when a backend fails (the next one is tried): the app learns of a revoked key.
    public let onModelError: @Sendable (Error) -> Void

    public init(rules: any NaturalLanguageEditService = RulesEditService(), models: [any NaturalLanguageEditService],
                onModelError: @escaping @Sendable (Error) -> Void = { _ in }) {
        self.rules = rules
        self.models = models
        self.onModelError = onModelError
    }

    /// Notes about pixel-dependent requests the rules already explained.
    static func isNote(_ s: String) -> Bool { s.contains("(use Auto") || s.contains("(use the Auto") }

    public func intent(for text: String) async throws -> EditIntent {
        var result = try await rules.intent(for: text)
        let leftovers = result.unrecognized.filter { !Self.isNote($0) }
        guard !leftovers.isEmpty else { return result }
        let notes = result.unrecognized.filter(Self.isNote)
        for model in models {
            try Task.checkCancellation()
            let answer: EditIntent
            do {
                answer = try await model.intent(for: leftovers.joined(separator: ", "))
            } catch {
                try Task.checkCancellation()
                onModelError(error)
                continue
            }
            let guarded = IntentGuard.constrain(answer.sanitized(), clauses: leftovers)
            result = IntentGuard.merge(rules: result, model: guarded.intent)
            result.unrecognized = notes + guarded.unrecognized
            return result
        }
        return result
    }
}
