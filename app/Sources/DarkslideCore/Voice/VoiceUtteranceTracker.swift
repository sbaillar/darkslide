import Foundation

/// What the recognizer heard so far in the current utterance.
public struct VoiceTranscript: Equatable, Sendable {
    public var text: String
    /// The recognizer finalized this text: the utterance ended.
    public var isFinal: Bool
    /// 0…1 when the recognizer reports it (finals usually do; partials often don't).
    public var confidence: Double?

    public init(_ text: String, isFinal: Bool = false, confidence: Double? = nil) {
        self.text = text
        self.isFinal = isFinal
        self.confidence = confidence
    }
}

/// Turns a stream of refining partials and finals into commands that fire once.
///
/// Finals always decide. A partial fires early only when it is `settled` (no
/// homophone or everyday word, and it can't still grow into another command:
/// "next" may become "next pick") and has stayed unchanged for `settle`.
/// Continuous listening also needs two or more command words in the partial, so
/// a single word heard in the room waits for the final; push-to-talk (the
/// speaker said they're talking to Darkslide) fires single words early too, and
/// its key release fires a settled pending partial at once (anything else waits
/// for the final).
///
/// Commands already fired in an utterance never fire again as later results
/// refine it ("five stars" → "five stars next" fires only "next"). A free-form
/// "edit …" fires only from the final.
public struct VoiceUtteranceTracker: Sendable {
    public enum Mode: Sendable { case continuous, pushToTalk }

    public var mode: Mode
    /// How long a settled partial must stay unchanged before it fires (seconds).
    /// Doubled when the recognizer reports no confidence for it.
    public var settle: Double
    /// Results reported below this confidence never fire.
    public var minConfidence: Double
    /// The user's own phrases (`VoicePhraseBook.actions`).
    public var custom: [String: VoiceAction] = [:]

    /// Commands fired so far in the current utterance.
    public private(set) var fired: [VoiceAction] = []
    /// A parsed partial waiting to settle, and when it was first heard.
    public private(set) var pending: Pending?
    /// The last final contradicted what its partials already fired: the text it heard.
    public private(set) var contradiction: String?
    /// What fired before that contradiction changed photos, so "undo" takes it back
    /// (a move or a zoom has nothing to undo, and undo would revert something older).
    public private(set) var contradictionUndoable = false
    /// Commands a push-to-talk release held back for the final (not settled yet). If
    /// the stream ends without a final they are lost: the controller says so, never runs them.
    public private(set) var heldBack: [VoiceAction]?

    public struct Pending: Equatable, Sendable {
        public var parse: VoiceParse
        public var since: Double
        /// Seconds it must stay unchanged; `nil`: it may fire only when speech ends.
        public var wait: Double?
    }

    public init(mode: Mode = .continuous, settle: Double = 0.18, minConfidence: Double = 0.35) {
        self.mode = mode
        self.settle = settle
        self.minConfidence = minConfidence
    }

    /// When `tick` should next run (nil: nothing can fire early).
    public var deadline: Double? { pending.flatMap { p in p.wait.map { p.since + $0 } } }

    /// Take a recognizer result heard at `now`; returns the commands to run now.
    public mutating func receive(_ t: VoiceTranscript, at now: Double) -> [VoiceAction] {
        let confident = t.confidence.map { $0 >= minConfidence } ?? true
        let parsed = confident ? VoiceCommandParser.analyze(t.text, custom: custom) : nil
        if t.isFinal {
            pending = nil
            heldBack = nil
            var out: [VoiceAction] = []
            if let parsed, extends(parsed.actions) {
                out = newActions(parsed.actions)
            } else if !fired.isEmpty {
                contradiction = t.text
                contradictionUndoable = fired.contains(where: \.isUndoable)
            }
            fired = []
            return out
        }
        guard let parsed, !parsed.actions.contains(where: { if case .edit = $0 { true } else { false } }) else {
            pending = nil
            return []
        }
        let early = parsed.settled && (mode == .pushToTalk || parsed.words >= 2)
        if pending?.parse.actions != parsed.actions {
            pending = Pending(parse: parsed, since: now, wait: early ? settle * (t.confidence == nil ? 2 : 1) : nil)
        } else if !early {
            // Same commands, but the speaker went on ("pick" → "pick the"): wait for more.
            pending?.wait = nil
        }
        return []
    }

    /// Fire a pending partial once it has settled.
    public mutating func tick(at now: Double) -> [VoiceAction] {
        guard let p = pending, let wait = p.wait, now >= p.since + wait else { return [] }
        pending = nil
        return newActions(p.parse.actions)
    }

    /// Push-to-talk released: speech is over, so a settled pending partial fires now
    /// whatever its wait. A final that follows fires only what this didn't. Anything
    /// else waits for the final, because the recognizer may not have caught up with
    /// the last words yet: "pick the" (→ "pick the next two"), "reject three" (→
    /// "reject three photos"), "undo three" (→ "undo three times"), "five" (→ "five stars").
    public mutating func flush() -> [VoiceAction] {
        guard let p = pending else { return [] }
        pending = nil
        guard p.parse.settled, !p.parse.continues else {
            // Only a push-to-talk release holds back: a continuous run's stream ending (the
            // audio input changed) drops an unsettled partial, as talk in the room should.
            if mode == .pushToTalk, extends(p.parse.actions), p.parse.actions.count > fired.count {
                heldBack = p.parse.actions
            }
            return []
        }
        heldBack = nil // a later settled partial ran it: nothing was lost
        return newActions(p.parse.actions)
    }

    /// Take what a release held back and no final followed (once).
    public mutating func takeHeldBack() -> [VoiceAction]? {
        defer { heldBack = nil }
        return heldBack
    }

    /// Take the contradiction note (once).
    public mutating func takeContradiction() -> String? {
        defer { contradiction = nil }
        return contradiction
    }

    public mutating func reset() {
        pending = nil
        fired = []
        contradiction = nil
        heldBack = nil
    }

    private func extends(_ actions: [VoiceAction]) -> Bool {
        actions.count >= fired.count && Array(actions.prefix(fired.count)) == fired
    }

    /// The part of `actions` not fired yet. A result that no longer starts with what
    /// already fired (the recognizer changed its mind) fires nothing more.
    private mutating func newActions(_ actions: [VoiceAction]) -> [VoiceAction] {
        guard extends(actions) else { return [] }
        let new = Array(actions.dropFirst(fired.count))
        fired = actions
        return new
    }
}
