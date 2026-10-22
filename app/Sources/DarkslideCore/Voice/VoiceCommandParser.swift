import Foundation

/// Jumps a spoken command can make that no single key does.
public enum VoiceJump: Equatable, Sendable {
    case first, last
    /// The next (`forward`) or previous photo that matches ("next pick", "previous reject").
    case seek(VoiceMatch, forward: Bool)

    public static let nextUnrated = VoiceJump.seek(.unrated, forward: true)
    public static let nextPick = VoiceJump.seek(.picked, forward: true)
}

/// What a seek looks for.
public enum VoiceMatch: Equatable, Sendable {
    case unrated, picked, rejected, unflagged
    /// Exactly this many stars ("next five star").
    case stars(Int)

    func matches(_ p: Photo) -> Bool {
        switch self {
        case .unrated: p.rating == 0
        case .picked: p.pick == .picked
        case .rejected: p.pick == .rejected
        case .unflagged: p.pick == .none
        case .stars(let n): p.rating == n
        }
    }

    /// "Next \(label)".
    var label: String {
        switch self {
        case .unrated: "unrated"
        case .picked: "pick"
        case .rejected: "reject"
        case .unflagged: "unflagged"
        case .stars(let n): String(repeating: "★", count: n)
        }
    }

    /// "No more \(adjective) photos".
    var adjective: String {
        switch self {
        case .unrated: "unrated"
        case .picked: "picked"
        case .rejected: "rejected"
        case .unflagged: "unflagged"
        case .stars(let n): "\(n)-star"
        }
    }
}

/// What one spoken command does to several photos at once ("reject the next 3").
public enum VoiceBatch: Equatable, Sendable {
    case rate(Int), pick, reject, unflag
    /// Extend the selection over them (like ⇧→).
    case select
    /// Paste the copied adjustments onto them.
    case apply
}

/// The photos a several-photo command covers: "the next 3" are the three after the
/// focused photo; "these 3" start with it.
public struct VoiceSpan: Equatable, Sendable {
    public var count: Int
    public var includesFocused: Bool

    public init(count: Int, includesFocused: Bool) {
        self.count = count
        self.includesFocused = includesFocused
    }
}

/// One spoken command. Each runs through the path its key or menu item uses
/// (`AppModel.perform(_:)`, `CommandCatalog.perform`, the ⌘K bar), so undo,
/// selection and saving behave exactly as they do for the keyboard.
public enum VoiceAction: Equatable, Sendable {
    /// A keyboard command (`AppModel.perform(_:)`).
    case key(KeyCommand)
    /// A menu command (`CommandCatalog.perform`, only while the menu item is enabled).
    case menu(MenuCommand)
    case jump(VoiceJump)
    /// "next 3" / "back 2": move this many photos (negative: back). One photo is `.key(.move…)`.
    case step(Int)
    /// "photo 12", "the fourth photo": the photo at this position (1 is the first).
    case goTo(Int)
    /// "reject the next 3", "rate these four three stars": one undo step.
    case batch(VoiceBatch, VoiceSpan)
    /// "undo 3": a key command run this many times.
    case repeated(KeyCommand, Int)
    /// "show picks": the filter bar's flag filter.
    case showFlags(PhotoFilter.PickFilter)
    /// "what can I say?": the Voice Commands sheet (true) or close it (false).
    case help(Bool)
    /// "stop listening": voice commands off.
    case stopListening
    /// "hide panels" / "show panels": the panels toggle, only when it changes something.
    case setPanelsHidden(Bool)
    /// "exit full screen": the full-screen toggle, only when in full screen.
    case setFullScreen(Bool)
    /// "edit <phrase>": the ⌘K Edit with Words instruction.
    case edit(String)

    /// What the HUD echoes (several-photo commands echo what they did instead).
    public var label: String {
        switch self {
        case .key(let k): Self.label(k)
        case .menu(.zoomIn): "Zoom in"
        case .menu(.zoomOut): "Zoom out"
        case .menu(.zoomFit): "Fit"
        case .menu(.zoomActual): "100%"
        case .menu(.copyAdjustments): "Copy adjustments"
        case .menu(.applyAdjustments): "Apply adjustments"
        case .menu(.autoAdjust): "Auto adjust"
        case .menu(.rotateLeft): "Rotate left"
        case .menu(.rotateRight): "Rotate right"
        case .menu(let m): m.rawValue
        case .jump(.first): "First"
        case .jump(.last): "Last"
        case .jump(.seek(let m, let forward)): "\(forward ? "Next" : "Previous") \(m.label)"
        case .step(let n): n > 0 ? "Forward \(n)" : "Back \(-n)"
        case .goTo(let n): "Photo \(n)"
        case .batch(let b, let s): "\(Self.label(b)) \(s.includesFocused ? "these" : "next") \(s.count)"
        case .repeated(let k, let n): "\(Self.label(k)) ×\(n)"
        case .showFlags(let f): Self.label(f)
        case .help(let shown): shown ? "Voice commands" : "Close voice commands"
        case .stopListening: "Stop listening"
        case .setPanelsHidden(let h): h ? "Hide panels" : "Show panels"
        case .setFullScreen(let f): f ? "Full screen" : "Exit full screen"
        case .edit(let text): "Edit: “\(text)”, press Return"
        }
    }

    private static func label(_ k: KeyCommand) -> String {
        switch k {
        case .rate(0): "No stars"
        case .rate(let n): String(repeating: "★", count: n)
        case .tag(let t): "\(t.name) tag"
        case .pick: "Pick"
        case .reject: "Reject"
        case .unflag: "Unflag"
        case .move(.right, _): "Next"
        case .move(.left, _): "Previous"
        case .gridMode: "Grid"
        case .viewerMode: "Viewer"
        case .toggleBeforeAfter: "Before / after"
        case .togglePinCompare: "Compare"
        case .toggleFullScreen: "Full screen"
        case .togglePanels: "Panels"
        case .undo: "Undo"
        case .redo: "Redo"
        default: "\(k)"
        }
    }

    private static func label(_ b: VoiceBatch) -> String {
        switch b {
        case .rate(let n): "Rate \(n == 0 ? "no stars" : String(repeating: "★", count: n))"
        case .pick: "Pick"
        case .reject: "Reject"
        case .unflag: "Unflag"
        case .select: "Select"
        case .apply: "Apply to"
        }
    }

    private static func label(_ f: PhotoFilter.PickFilter) -> String {
        switch f {
        case .all: "Show all"
        case .picked: "Show picks"
        case .rejected: "Show rejects"
        case .unflagged: "Show unflagged"
        case .unrejected: "Show not rejected"
        }
    }

    /// Ratings and flags: what auto-advance follows.
    var isCull: Bool {
        switch self {
        case .key(.rate), .key(.pick), .key(.reject), .key(.unflag): true
        default: false
        }
    }

    var isRating: Bool {
        if case .key(.rate) = self { return true }
        return false
    }
}

extension VoiceAction {
    /// A position: what a rating may not follow ("the last one", "go to photo 3, five").
    var isPosition: Bool {
        switch self {
        case .goTo, .jump(.first), .jump(.last): true
        default: false
        }
    }

    /// It changes photos (undo takes it back): what the "say undo" note may suggest undoing.
    var isUndoable: Bool {
        switch self {
        case .key(.rate), .key(.pick), .key(.reject), .key(.unflag), .key(.tag), .menu(.autoAdjust),
             .menu(.applyAdjustments): true
        case .batch(let b, _): b != .select
        default: false
        }
    }

    /// A name that survives catalog edits (custom phrases are stored by it); `nil` for
    /// actions that take a count.
    var stableID: String? {
        switch self {
        case .key(.rate(let n)): "rate-\(n)"
        case .key(.tag(let t)): "tag-\(t.name.lowercased())"
        case .key(.move(.right, _)): "next"
        case .key(.move(.left, _)): "previous"
        case .key(let k): "key-\(k)"
        case .menu(let m): "menu-\(m.rawValue)"
        case .jump(.first): "first"
        case .jump(.last): "last"
        case .jump(.seek(let m, let forward)): "\(forward ? "next" : "previous")-\(m.adjective)"
        case .showFlags(let f): "show-\(f.rawValue.lowercased())"
        case .help(let shown): shown ? "help" : "close-help"
        case .stopListening: "stop-listening"
        case .setPanelsHidden(let h): h ? "hide-panels" : "show-panels"
        case .setFullScreen(let f): f ? "enter-full-screen" : "exit-full-screen"
        case .step, .goTo, .batch, .repeated, .edit: nil
        }
    }
}

/// A parsed utterance.
public struct VoiceParse: Equatable, Sendable {
    public var actions: [VoiceAction]
    /// Words that carried commands (fillers not counted).
    public var words: Int
    /// A partial result saying this may fire before the final: nothing in it is a
    /// homophone or everyday word, and it can't still grow into another command.
    public var settled: Bool
    /// It ends on a word that leads into more ("pick the", "five next", "reject"): a
    /// push-to-talk release waits for the final instead of firing it.
    public var continues = false
}

/// Spoken text → commands. Only a whole utterance that is entirely commands
/// matches (talk in the room never fires anything): "five", "next", "five next",
/// "next 3", "go to photo 12", "reject the next 3", "edit warmer and a bit brighter".
///
/// Deterministic: fixed phrases (`catalog`, plus the user's own) and a small grammar
/// for counts and positions; the longest match wins at each word. When in doubt the
/// grammar wants more explicit wording, and an utterance it can't read for sure is
/// nothing at all:
/// - "next 3", "back two", "skip 4 photos": move that many photos. A count after
///   "next" moves only when it ends the phrase or "photos" follows ("five next four
///   next" still rates). "three" alone is three stars; "next three stars" is next,
///   then three stars. A homophone ("next to", "skip for") moves only before "photos".
/// - Positions say so: "go to 12", "go to photo 12", "jump to number 12", "go to the
///   fourth", "the fourth photo", or "photo 12" on its own. "forward to the fourth" is
///   a position (the "to" names a destination); "forward four" moves four. A bare
///   "the second one", "number one" or "a second" is nothing.
/// - Several photos need a count of two or more said as "the next N", "next N
///   photos", "these N" or "N photos": "reject the next 3", "pick these three", "rate
///   the next four three stars", "select the next 3", "apply to the next 3". "The next
///   N" are the N photos after the focused one; "these N" and "N photos" start with it.
///   "pick next one" stays pick, then next. A verb before a count it can't take
///   ("reject the first 3", "pick the previous two", "pick next 3") is nothing.
/// - "undo 3 times" / "undo 3 steps"; "undo five" is undo, then five stars.
/// - A rating may not be followed by a count or a position, nor a position by a rating:
///   "two four", "for the next two" and "the last one" are sentences.
public enum VoiceCommandParser {
    /// The commands in `text`, in order; `nil` unless every word is part of one.
    /// `custom`: the user's own phrases (keys from `normalize`).
    public static func parse(_ text: String, custom: [String: VoiceAction] = [:]) -> [VoiceAction]? {
        analyze(text, custom: custom)?.actions
    }

    /// `parse`, plus whether a partial result saying this may already fire.
    public static func analyze(_ text: String, custom: [String: VoiceAction] = [:]) -> VoiceParse? {
        let words = tokens(text)
        if words.first == "edit" {
            let phrase = editPhrase(text)
            return phrase.isEmpty ? nil : VoiceParse(actions: [.edit(phrase)], words: words.count, settled: false)
        }
        let ws = stripFillers(words)
        guard !ws.contains(unreadable) else { return nil }
        var out: [VoiceAction] = []
        // Ending on a filler ("pick the", "rate it"): the speaker isn't done.
        var settled = !(words.last.map { fillers.contains($0) || markers.contains($0) } ?? false)
        var i = 0
        while i < ws.count {
            if skipped.contains(ws[i]) {
                i += 1
                continue
            }
            let candidates = [phraseMatch(ws, at: i, custom: custom), ratingMatch(ws, at: i), stepMatch(ws, at: i),
                              positionMatch(ws, at: i), batchMatch(ws, at: i), repeatMatch(ws, at: i)]
            guard let m = candidates.compactMap(\.self).max(by: { $0.length < $1.length }), let action = m.action else {
                return nil
            }
            if let last = out.last {
                // A rating then a number, a count or a place ("two four", "one second", "for
                // the next two"), or a place then a rating ("the last one"): a sentence.
                if last.isRating, action.isRating || action.isPosition || action.takesCount { return nil }
                if last.isPosition, action.isRating { return nil }
                // "skip to the next": a homophone after a move is a word, not a rating.
                if last.isMove, action.isRating, homophones.contains(ws[i]) { return nil }
            }
            let spoken = ws[i..<(i + m.length)]
            if spoken.contains(where: partialUnsafe.contains) { settled = false }
            i += m.length
            // The speaker may not be done: "next" → "next pick", "five" → "five stars",
            // "next 3" → "next 30", "clear" → "clear flag".
            if ws[i...].allSatisfy(skipped.contains), m.openEnded { settled = false }
            out.append(action)
        }
        guard !out.isEmpty else { return nil }
        let continues = words.last.map { continuations.contains($0) || batchVerbs[$0] != nil || ratingPrefixes.contains($0) }
            ?? false
        return VoiceParse(actions: out, words: ws.count { !skipped.contains($0) }, settled: settled, continues: continues)
    }

    /// Words, lowercased, punctuation gone ("100%" → "100 percent", "full-screen" → "full screen").
    static func tokens(_ text: String) -> [String] {
        var s = text.lowercased()
        s = s.replacingOccurrences(of: "%", with: " percent ")
        s = s.replacingOccurrences(of: "&", with: " and ")
        s = s.replacingOccurrences(of: "#", with: " number ")
        s = s.replacingOccurrences(of: "-", with: " ")
        s = s.replacingOccurrences(of: "’", with: "").replacingOccurrences(of: "'", with: "")
        let kept = s.unicodeScalars.map { CharacterSet.alphanumerics.contains($0) ? Character($0) : " " }
        return String(kept).split(separator: " ").map(String.init)
    }

    /// A phrase as the matcher sees it: words without fillers ("bin it" → "bin").
    public static func normalize(_ phrase: String) -> String { stripFillers(tokens(phrase)).joined(separator: " ") }

    /// A built-in phrase's table key: separators dropped too ("first photo" → "first").
    private static func key(_ phrase: String) -> String {
        stripFillers(tokens(phrase)).filter { !skipped.contains($0) }.joined(separator: " ")
    }

    /// The instruction after "edit", as spoken (case kept, edge punctuation dropped).
    private static func editPhrase(_ text: String) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines.union(.punctuationCharacters))
        guard let r = trimmed.range(of: "edit", options: [.caseInsensitive, .anchored]) else { return "" }
        return trimmed[r.upperBound...].trimmingCharacters(in: .whitespacesAndNewlines.union(.punctuationCharacters))
    }

    /// Words that carry no command ("next photo please", "rate it five", "five then next").
    static let fillers: Set<String> = ["please", "and", "then", "the", "photo", "picture", "image", "shot", "it", "now",
                                       "ok", "okay", "um", "uh", "go", "a"]
    /// Words that name a photo: before a number they make it a position ("go to photo
    /// 12"), after an ordinal they make it one ("the fourth photo").
    private static let markers: Set<String> = ["photo", "picture", "image", "shot", "number"]
    /// Words a push-to-talk release waits past (with the batch verbs and "rate").
    private static let continuations: Set<String> = [
        "the", "a", "next", "following", "previous", "prev", "back", "forward", "skip", "to", "too", "for", "and", "then",
        "these", "those", "go", "jump", "undo", "redo",
    ]

    // Tokens `stripFillers` makes:
    /// "go to" / "jump to" before a number or ordinal: what follows is a position.
    private static let goTo = "→"
    /// A marker before a number where a position may be: "go to photo 12", "photo 12".
    private static let position = "#"
    /// A marker after an ordinal: "the fourth photo".
    private static let noun = "§"
    /// "the" before "next": "reject the next 3" names photos.
    private static let the = "the"
    /// A marker that is just a word between commands: "next photo, five".
    private static let separator = "·"
    /// A number after a marker where no position can be ("number one", "pick photo 3"): nothing.
    private static let unreadable = "?"
    /// Tokens no command starts with.
    private static let skipped: Set<String> = [the, separator]

    private static func stripFillers(_ words: [String]) -> [String] {
        var out: [String] = []
        var i = 0
        while i < words.count {
            let w = words[i]
            // "go to next": "to" is part of "go to", not "two"; "go to 12" is a position.
            if w == "go" || w == "jump", i + 1 < words.count, words[i + 1] == "to" {
                i += 2
                var j = i
                while j < words.count, words[j] == "the" || (markers.contains(words[j]) && j + 1 < words.count
                                                              && markers.contains(words[j + 1])) { j += 1 }
                if isCount(words, at: j) || ordinal(words, at: j) != nil
                    || (j < words.count && markers.contains(words[j]) && isCount(words, at: j + 1)) {
                    out.append(goTo)
                }
                continue
            }
            if markers.contains(w) {
                if w != "number", i > 0, count(words, at: i - 1)?.length == 1, !homophones.contains(words[i - 1]) {
                    out.append(w) // "next 3 photo": the noun (recognizers drop the plural)
                } else if i + 1 < words.count, markers.contains(words[i + 1]) {
                    // "photo number 7": the last marker decides.
                } else if isCount(words, at: i + 1) {
                    let count = count(words, at: i + 1)!.length
                    if out.last == goTo || out.last == "to" || out.last == position {
                        if out.last != position { out.append(position) }
                    } else if out.isEmpty, w != "number", i + 1 + count == words.count {
                        out.append(position) // "photo 12" on its own
                    } else if let last = out.last, forwardWords.contains(last) || backWords.contains(last) {
                        out.append(separator) // "next photo, five": next, then five stars
                    } else {
                        out.append(unreadable)
                    }
                } else if i > 0, ordinal(words, at: i - 1) != nil {
                    out.append(noun)
                } else {
                    out.append(separator)
                }
            } else if w == "the", i + 1 < words.count, ["next", "following"].contains(words[i + 1]) {
                out.append(the)
            } else if !fillers.contains(w) {
                out.append(w)
            }
            i += 1
        }
        return out
    }

    /// A count that isn't a rating ("photo 3", not "photo, 3 stars").
    private static func isCount(_ words: [String], at i: Int) -> Bool {
        guard let c = count(words, at: i) else { return false }
        return !isStars(words, at: i + c.length)
    }

    private static func isStars(_ ws: [String], at i: Int) -> Bool { i < ws.count && ratingSuffixes.contains(ws[i]) }

    // MARK: - The table

    /// Fixed phrases (after fillers are removed). Longest match wins, so "next one"
    /// is next (not next + one star) and "clear flag" is unflag (not clear rating).
    /// Built from `catalog`, so the Voice Commands sheet lists exactly what is heard.
    static let phrases: [String: VoiceAction] = {
        var p: [String: VoiceAction] = [:]
        func add(_ spoken: String, _ action: VoiceAction) {
            let k = key(spoken)
            precondition(p[k] == nil || p[k] == action, "“\(spoken)” means two commands")
            p[k] = action
        }
        for group in catalog {
            for c in group.commands {
                guard let action = c.action else { continue }
                for s in c.phrases + c.hidden { add(s, action) }
            }
        }
        // "next five star": the next photo with exactly five stars (singular "star":
        // "next five stars" is next, then five stars). Not "next one star": "next one"
        // is next, and settles at once.
        for (n, word) in ["1", "two", "three", "four", "five"].enumerated() {
            for (dir, forward) in [("next", true), ("previous", false), ("prev", false)] {
                for number in Set([word, "\(n + 1)"]) { add("\(dir) \(number) star", .jump(.seek(.stars(n + 1), forward: forward))) }
            }
        }
        return p
    }()

    /// Proper word prefixes of the phrases ("next" of "next pick", "clear" of "clear flag").
    static let phrasePrefixes: Set<String> = prefixes(of: phrases.keys)

    private static func prefixes(of keys: some Sequence<String>) -> Set<String> {
        Set(keys.flatMap { key -> [String] in
            let w = key.split(separator: " ")
            return (1..<max(w.count, 1)).map { w.prefix($0).joined(separator: " ") }
        })
    }

    /// Words a partial never fires on: homophones and words common in everyday speech
    /// ("for the record", "to be honest", "back in the day"). The final result decides.
    static let partialUnsafe: Set<String> = [
        "to", "too", "tu", "for", "fore", "won", "wan", "tree", "fife", "fiv", "read", "pic", "pik", "necks", "nex",
        "view", "back", "skip", "forward", "single", "closer", "auto", "fit", "clear", "first", "last", "prev", "set",
        "give", "help", "these", "those",
    ]

    private static let longestPhrase = phrases.keys.map { $0.split(separator: " ").count }.max() ?? 1

    private static func phraseMatch(_ ws: [String], at i: Int, custom: [String: VoiceAction]) -> Match? {
        let longest = max(longestPhrase, custom.keys.map { $0.split(separator: " ").count }.max() ?? 0)
        for n in stride(from: min(longest, ws.count - i), through: 1, by: -1) {
            let key = ws[i..<(i + n)].joined(separator: " ")
            if let a = custom[key] ?? phrases[key] {
                let open = phrasePrefixes.contains(key) || (!custom.isEmpty && prefixes(of: custom.keys).contains(key))
                return Match(action: a, length: n, openEnded: open)
            }
        }
        return nil
    }

    private struct Match {
        /// `nil`: words the grammar can't read for sure ("reject the first 3"): the whole utterance is nothing.
        var action: VoiceAction?
        var length: Int
        /// It ends the utterance but may still grow ("five" → "five stars", "next 3" → "next 30").
        var openEnded = false
    }

    // MARK: - Numbers

    /// Spoken ratings and what recognizers hear for them ("for", "to", "won", "fife").
    static let numbers: [String: Int] = [
        "zero": 0, "0": 0, "one": 1, "1": 1, "won": 1, "wan": 1, "two": 2, "2": 2, "to": 2, "too": 2, "tu": 2,
        "three": 3, "3": 3, "tree": 3, "four": 4, "4": 4, "for": 4, "fore": 4, "five": 5, "5": 5, "fife": 5, "fiv": 5,
    ]
    /// What recognizers hear for small numbers: counts only before "photos" ("skip for" is nothing).
    private static let homophones: Set<String> = ["to", "too", "tu", "for", "fore", "won", "wan", "tree", "fife", "fiv"]
    private static let units: [String: Int] = [
        "six": 6, "seven": 7, "eight": 8, "nine": 9, "ten": 10, "eleven": 11, "twelve": 12, "thirteen": 13,
        "fourteen": 14, "fifteen": 15, "sixteen": 16, "seventeen": 17, "eighteen": 18, "nineteen": 19,
    ]
    private static let tens: [String: Int] = [
        "twenty": 20, "thirty": 30, "forty": 40, "fifty": 50, "sixty": 60, "seventy": 70, "eighty": 80, "ninety": 90,
    ]
    /// Units a compound takes ("twenty one"; not the homophones: "twenty to" is a time).
    private static let compoundUnits: [String: Int] = [
        "one": 1, "two": 2, "three": 3, "four": 4, "five": 5, "six": 6, "seven": 7, "eight": 8, "nine": 9,
    ]
    private static let ordinals: [String: Int] = [
        "first": 1, "second": 2, "third": 3, "fourth": 4, "fifth": 5, "sixth": 6, "seventh": 7, "eighth": 8,
        "ninth": 9, "tenth": 10, "eleventh": 11, "twelfth": 12, "thirteenth": 13, "fourteenth": 14, "fifteenth": 15,
        "sixteenth": 16, "seventeenth": 17, "eighteenth": 18, "nineteenth": 19, "twentieth": 20, "thirtieth": 30,
        "fortieth": 40, "fiftieth": 50, "sixtieth": 60, "seventieth": 70, "eightieth": 80, "ninetieth": 90,
    ]
    /// The largest count or position heard (more is a misrecognition, and keeps arithmetic small).
    static let maxCount = 999
    /// The most undo steps one phrase takes.
    static let maxRepeat = 20

    /// A count at `i`: digits up to `maxCount`, words up to ninety-nine, and what
    /// recognizers hear for small ones.
    static func count(_ ws: [String], at i: Int) -> (value: Int, length: Int)? {
        guard i < ws.count else { return nil }
        let w = ws[i]
        if !w.isEmpty, w.allSatisfy(\.isASCIIDigit) {
            guard w.count <= 3, let n = Int(w) else { return nil }
            return (n, 1)
        }
        if let t = tens[w] {
            if i + 1 < ws.count, let u = compoundUnits[ws[i + 1]] { return (t + u, 2) }
            return (t, 1)
        }
        return (numbers[w] ?? units[w]).map { ($0, 1) }
    }

    /// An ordinal at `i`: "fourth", "4th", "twenty first".
    static func ordinal(_ ws: [String], at i: Int) -> (value: Int, length: Int)? {
        guard i < ws.count else { return nil }
        let w = ws[i]
        if let t = tens[w], i + 1 < ws.count, let o = ordinals[ws[i + 1]], o < 10 { return (t + o, 2) }
        if let o = ordinals[w] { return (o, 1) }
        for suffix in ["st", "nd", "rd", "th"] where w.hasSuffix(suffix) {
            let digits = w.dropLast(2)
            if !digits.isEmpty, digits.count <= 3, digits.allSatisfy(\.isASCIIDigit), let n = Int(digits), n > 0 {
                return (n, 1)
            }
        }
        return nil
    }

    private static let ratingPrefixes: Set<String> = ["rate", "rating", "rated", "give", "set"]
    private static let ratingSuffixes: Set<String> = ["star", "stars"]
    /// What may follow a count ("next three photos").
    private static let nouns: Set<String> = ["photos", "pictures", "images", "shots", "frames", "ones",
                                             "photo", "picture", "image", "shot", "frame"]

    /// `<0–5> [stars]` at `i`.
    private static func rating(_ ws: [String], at i: Int) -> (value: Int, end: Int, stars: Bool)? {
        guard i < ws.count, let n = numbers[ws[i]] else { return nil }
        return isStars(ws, at: i + 1) ? (n, i + 2, true) : (n, i + 1, false)
    }

    /// `[rate] <0–5> [stars]`.
    private static func ratingMatch(_ ws: [String], at i: Int) -> Match? {
        let j = ratingPrefixes.contains(ws[i]) ? i + 1 : i
        guard let r = rating(ws, at: j) else { return nil }
        return Match(action: .key(.rate(r.value)), length: r.end - i, openEnded: !r.stars)
    }

    // MARK: - Counts and positions

    private static let forwardWords: Set<String> = ["next", "forward", "forwards", "ahead", "skip", "advance"]
    private static let backWords: Set<String> = ["back", "backward", "backwards", "previous", "prev"]
    private static let moveWords: Set<String> = ["move", "jump"]

    /// `[move] next|back <count> [photos]`: "next 3", "back two", "skip 4 photos". The
    /// count must end the phrase or be followed by a noun ("five next four next" rates);
    /// a homophone needs the noun ("next to" is nothing, "next to photos"… is two).
    private static func stepMatch(_ ws: [String], at i: Int) -> Match? {
        var j = moveWords.contains(ws[i]) ? i + 1 : i
        guard j < ws.count, forwardWords.contains(ws[j]) || backWords.contains(ws[j]) else { return nil }
        let forward = forwardWords.contains(ws[j])
        j += 1
        guard let c = count(ws, at: j), c.value >= 1, c.value <= maxCount else { return nil }
        let heard = ws[j]
        j += c.length
        var open = true
        if j < ws.count, nouns.contains(ws[j]) {
            j += 1
            open = false
        } else if j < ws.count {
            return nil
        } else if homophones.contains(heard) {
            return Match(action: nil, length: j - i) // "skip for", "back to": maybe the start of a sentence
        }
        let action: VoiceAction = c.value == 1 ? .key(.move(forward ? .right : .left, extend: false))
            : .step(forward ? c.value : -c.value)
        return Match(action: action, length: j - i, openEnded: open)
    }

    /// "go to 12" / "go to photo 12" (`→`, `#` from `stripFillers`), "photo 12" alone,
    /// "go to the fourth", "the fourth photo" (`§`), and "forward to the fourth" /
    /// "skip to 10" (a destination, not a count).
    private static func positionMatch(_ ws: [String], at i: Int) -> Match? {
        var j = i
        var explicit = false
        if ws[j] == goTo {
            j += 1
            explicit = true
        } else {
            if moveWords.contains(ws[j]) { j += 1 }
            if j < ws.count, forwardWords.contains(ws[j]) || backWords.contains(ws[j]) { j += 1 }
            if j > i, j < ws.count, ws[j] == "to" {
                j += 1
                explicit = true
            }
        }
        guard j < ws.count else { return nil }
        let n: Int
        var open = false
        if ws[j] == position, let c = count(ws, at: j + 1) {
            n = c.value
            j += 1 + c.length
            open = j == ws.count
        } else if let o = ordinal(ws, at: j) {
            n = o.value
            j += o.length
            if j < ws.count, ws[j] == noun {
                j += 1
            } else if !explicit {
                return nil // "the second", "a second": not a place
            }
        } else if explicit, let c = count(ws, at: j), !isStars(ws, at: j + c.length) {
            n = c.value
            j += c.length
            open = true
        } else {
            return nil
        }
        guard (1...maxCount).contains(n) else { return nil }
        return Match(action: n == 1 ? .jump(.first) : .goTo(n), length: j - i, openEnded: open)
    }

    /// Which photos a several-photo command names, at `i` (after the verb): `[to|for|on]`
    /// then "the next N [photos]", "next N photos", "these N [photos]" or "N photos".
    /// `nil` result: no span here. `.some(nil)`: the start of one the grammar refuses.
    private static func span(_ ws: [String], at i: Int) -> (span: VoiceSpan, end: Int, open: Bool)?? {
        var j = i
        if j < ws.count, ["to", "for", "on", "onto"].contains(ws[j]), j + 1 < ws.count,
           [the, "next", "following", "these", "those"].contains(ws[j + 1]) || count(ws, at: j + 1) != nil {
            j += 1
        }
        guard j < ws.count else { return nil }
        var explicit = false
        if ws[j] == the {
            j += 1
            explicit = true
        }
        guard j < ws.count else { return nil }
        let includes: Bool
        switch ws[j] {
        case "next", "following": includes = false
        case "these", "those":
            includes = true
            explicit = true
        case "previous", "prev", "back", "first", "last":
            // "reject the previous 3", "pick the first two": no span the grammar reads.
            return count(ws, at: j + 1) != nil ? .some(nil) : nil
        default:
            // "reject 3 photos": these three.
            guard let c = count(ws, at: j), j + c.length < ws.count, nouns.contains(ws[j + c.length]) else { return nil }
            guard c.value >= 1, c.value <= maxCount else { return .some(nil) }
            return (VoiceSpan(count: c.value, includesFocused: true), j + c.length + 1, false)
        }
        // "pick next three stars": pick, next, three stars.
        guard let c = count(ws, at: j + 1), !isStars(ws, at: j + 1 + c.length) else { return nil }
        j += 1 + c.length
        let hasNoun = j < ws.count && nouns.contains(ws[j])
        // "pick next one": pick, then next (one photo is the old chain).
        if c.value == 1, !includes, !explicit || !hasNoun, !homophones.contains(ws[j - 1]) { return nil }
        guard c.value >= 2, c.value <= maxCount, explicit || hasNoun,
              !homophones.contains(ws[j - c.length]) || hasNoun else { return .some(nil) }
        let span = VoiceSpan(count: c.value, includesFocused: includes)
        return hasNoun ? (span, j + 1, false) : (span, j, true)
    }

    static let batchVerbs: [String: VoiceBatch] = [
        "pick": .pick, "picked": .pick, "pic": .pick, "reject": .reject, "rejected": .reject, "unflag": .unflag,
        "un flag": .unflag, "select": .select, "apply": .apply, "paste": .apply, "apply settings": .apply,
        "apply adjustments": .apply, "apply edits": .apply, "paste settings": .apply, "paste adjustments": .apply,
        "paste edits": .apply,
    ]

    /// "reject the next 3", "apply to the next 3", "rate the next four three stars".
    private static func batchMatch(_ ws: [String], at i: Int) -> Match? {
        for n in [2, 1] where i + n <= ws.count {
            guard let verb = batchVerbs[ws[i..<(i + n)].joined(separator: " ")], let found = span(ws, at: i + n) else { continue }
            guard let s = found, !isStars(ws, at: s.end) else { return Match(action: nil, length: n + 1) }
            if s.span.count == 1 {
                // "pick 1 photo": this photo, the plain command.
                let one: KeyCommand? = switch verb {
                case .pick: .pick
                case .reject: .reject
                case .unflag: .unflag
                default: nil
                }
                return Match(action: one.map(VoiceAction.key), length: s.end - i)
            }
            return Match(action: .batch(verb, s.span), length: s.end - i, openEnded: s.open)
        }
        guard ratingPrefixes.contains(ws[i]), let found = span(ws, at: i + 1) else { return nil }
        guard let s = found, let r = rating(ws, at: s.end) else { return Match(action: nil, length: 2) }
        let action: VoiceAction = s.span.count == 1 ? .key(.rate(r.value)) : .batch(.rate(r.value), s.span)
        return Match(action: action, length: r.end - i, openEnded: !r.stars)
    }

    /// "undo 3 times", "redo 2 steps" ("undo five" is undo, then five stars).
    private static func repeatMatch(_ ws: [String], at i: Int) -> Match? {
        let command: KeyCommand
        switch ws[i] {
        case "undo": command = .undo
        case "redo": command = .redo
        default: return nil
        }
        // "undo the last two": not a count the grammar reads ("undo last" alone is undo).
        if i + 1 < ws.count, ws[i + 1] == "last", count(ws, at: i + 2) != nil { return Match(action: nil, length: 3) }
        guard let c = count(ws, at: i + 1), c.value >= 1, c.value <= maxRepeat,
              i + 1 + c.length < ws.count, ["times", "time", "steps", "step"].contains(ws[i + 1 + c.length])
        else { return nil }
        return Match(action: c.value == 1 ? .key(command) : .repeated(command, c.value), length: c.length + 2)
    }

    /// Words worth biasing the recognizer toward (its contextual strings).
    public static var vocabulary: [String] {
        Array(Set(phrases.keys.filter { !$0.contains(where: \.isNumber) })
            .union(["one", "two", "three", "four", "five", "zero", "stars", "edit", "photo", "number", "these",
                    "following", "times", "second", "third", "fourth", "fifth"])).sorted()
    }
}

extension VoiceAction {
    /// A move: what a homophone rating may not follow ("skip to the next").
    var isMove: Bool {
        switch self {
        case .key(.move), .step: true
        default: false
        }
    }

    /// It takes a count: what may not follow a rating ("for the next two").
    var takesCount: Bool {
        switch self {
        case .step, .batch, .repeated: true
        default: false
        }
    }
}

extension Character {
    fileprivate var isASCIIDigit: Bool { isASCII && isNumber }
}
