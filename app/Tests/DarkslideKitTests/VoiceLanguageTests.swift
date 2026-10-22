import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

private let next = VoiceAction.key(.move(.right, extend: false))
private let previous = VoiceAction.key(.move(.left, extend: false))
private let undo = VoiceAction.key(.undo)
private func rate(_ n: Int) -> VoiceAction { .key(.rate(n)) }
private func nextN(_ n: Int) -> VoiceSpan { VoiceSpan(count: n, includesFocused: false) }
private func these(_ n: Int) -> VoiceSpan { VoiceSpan(count: n, includesFocused: true) }
private func seek(_ m: VoiceMatch, forward: Bool = true) -> VoiceAction { .jump(.seek(m, forward: forward)) }

@Suite struct VoiceLanguageParserTests {
    /// Utterance → commands. The grammar's whole contract in one table.
    static let cases: [(String, [VoiceAction])] = [
        // Counts: a number after next/back ends the phrase or is followed by "photos".
        ("next 3", [.step(3)]), ("next three", [.step(3)]), ("next three photos", [.step(3)]), ("Next 3 pictures.", [.step(3)]),
        ("forward 3", [.step(3)]), ("skip 4", [.step(4)]), ("skip four", [.step(4)]), ("skip for photos", [.step(4)]),
        ("back 2", [.step(-2)]), ("previous two", [.step(-2)]), ("go back two photos", [.step(-2)]),
        ("back 1", [previous]), ("next twelve", [.step(12)]), ("next twenty one", [.step(21)]),
        ("move forward 5", [.step(5)]), ("go ahead 6", [.step(6)]),
        // Positions say so.
        ("go to the 4th photo", [.goTo(4)]), ("fourth photo", [.goTo(4)]), ("the fourth photo", [.goTo(4)]),
        ("go to the fourth", [.goTo(4)]), ("photo 12", [.goTo(12)]), ("photo twelve", [.goTo(12)]),
        ("go to number 12", [.goTo(12)]), ("go to 12", [.goTo(12)]), ("go to photo number 7", [.goTo(7)]),
        ("picture 40", [.goTo(40)]), ("twenty first photo", [.goTo(21)]), ("go to the 22nd", [.goTo(22)]),
        ("jump to 30", [.goTo(30)]), ("jump to photo 9", [.goTo(9)]), ("go to the first photo", [.jump(.first)]),
        // "to the fourth" is a destination, "four" is a count.
        ("go forward to the fourth", [.goTo(4)]), ("skip to 10", [.goTo(10)]), ("forward four", [.step(4)]),
        ("back to the first", [.jump(.first)]), ("first", [.jump(.first)]), ("first one", [.jump(.first)]),
        ("last", [.jump(.last)]), ("go to the last photo", [.jump(.last)]), ("go to photo 12, pick", [.goTo(12), .key(.pick)]),
        // Ratings stay ratings.
        ("three", [rate(3)]), ("3", [rate(3)]), ("tree", [rate(3)]), ("next three stars", [next, rate(3)]),
        ("five next four next", [rate(5), next, rate(4), next]), ("photo, 3 stars", [rate(3)]),
        ("rate it four", [rate(4)]), ("next photo five", [next, rate(5)]), ("previous photo two", [previous, rate(2)]),
        // One photo stays the old chain (review-g H1).
        ("pick next one", [.key(.pick), next]), ("Reject. Next one.", [.key(.reject), next]),
        ("reject the next one", [.key(.reject), next]), ("unflag the next one", [.key(.unflag), next]),
        ("pick next", [.key(.pick), next]), ("reject 3", [.key(.reject), rate(3)]),
        // A singular noun after a count is still the noun (review-g2 M2').
        ("next 3 photo", [.step(3)]), ("back two picture", [.step(-2)]), ("pick 1 photo", [.key(.pick)]),
        ("reject one photo", [.key(.reject)]), ("pick next 1 photo", [.key(.pick), next]),
        ("rate 1 photo five stars", [rate(5)]), ("reject 3 photo", [.batch(.reject, these(3))]),
        ("back to photo 3", [.goTo(3)]), ("pick, reject the next 3", [.key(.pick), .batch(.reject, nextN(3))]),
        // Undo with a count says "times" or "steps" (review-g H2).
        ("undo five", [undo, rate(5)]), ("undo 3", [undo, rate(3)]), ("undo for", [undo, rate(4)]),
        ("undo three times", [.repeated(.undo, 3)]), ("undo 3 times", [.repeated(.undo, 3)]),
        ("redo 2 steps", [.repeated(.redo, 2)]), ("undo 1 time", [undo]),
        // Several photos: two or more, said explicitly.
        ("reject the next 3", [.batch(.reject, nextN(3))]), ("reject next three photos", [.batch(.reject, nextN(3))]),
        ("pick these three", [.batch(.pick, these(3))]), ("pick the next two", [.batch(.pick, nextN(2))]),
        ("unflag the next 4", [.batch(.unflag, nextN(4))]), ("reject 3 photos", [.batch(.reject, these(3))]),
        ("pick next 2 photos", [.batch(.pick, nextN(2))]), ("pick those two", [.batch(.pick, these(2))]),
        ("reject the following 3", [.batch(.reject, nextN(3))]),
        ("select the next 3", [.batch(.select, nextN(3))]), ("select these 4", [.batch(.select, these(4))]),
        ("apply to the next 3", [.batch(.apply, nextN(3))]), ("apply settings to these 4", [.batch(.apply, these(4))]),
        ("paste to the next five", [.batch(.apply, nextN(5))]),
        ("rate the next four three stars", [.batch(.rate(3), nextN(4))]), ("rate these two five stars", [.batch(.rate(5), these(2))]),
        ("give the next 3 one star", [.batch(.rate(1), nextN(3))]), ("rate 3 photos five stars", [.batch(.rate(5), these(3))]),
        ("pick next three stars", [.key(.pick), next, rate(3)]), ("reject the next 3 then next", [.batch(.reject, nextN(3)), next]),
        // Seeks.
        ("next unflagged", [seek(.unflagged)]), ("next reject", [seek(.rejected)]), ("next rejected", [seek(.rejected)]),
        ("previous pick", [seek(.picked, forward: false)]), ("previous unrated", [seek(.unrated, forward: false)]),
        ("next five star", [seek(.stars(5))]), ("next 5 star photo", [seek(.stars(5))]), ("next 1 star", [seek(.stars(1))]),
        ("previous three star", [seek(.stars(3), forward: false)]), ("next five stars", [next, rate(5)]),
        ("next pick", [.jump(.nextPick)]), ("next unrated", [.jump(.nextUnrated)]),
        // Filter, view, edit, voice.
        ("show picks", [.showFlags(.picked)]), ("show rejects", [.showFlags(.rejected)]), ("show all", [.showFlags(.all)]),
        ("show unflagged", [.showFlags(.unflagged)]), ("compare", [.key(.togglePinCompare)]),
        ("before after", [.key(.toggleBeforeAfter)]), ("rotate left", [.menu(.rotateLeft)]),
        ("rotate right", [.menu(.rotateRight)]), ("auto", [.menu(.autoAdjust)]),
        ("undo that", [undo]), ("undo last", [undo]), ("copy settings", [.menu(.copyAdjustments)]),
        ("apply settings", [.menu(.applyAdjustments)]), ("stop listening", [.stopListening]),
        ("What can I say?", [.help(true)]), ("help", [.help(true)]), ("close help", [.help(false)]),
    ]

    /// Talk in the room, made only of words the grammar knows: nothing.
    static let roomTalk = [
        // Ordinals and "number" without "go to" or "photo" (review-g H3, M2).
        "the second one", "the third one", "the last one", "fifth one", "twenty first one", "a second", "second",
        "the fifth", "third", "the 4th", "the twenty first", "number one", "number two", "#12", "number 12",
        "go to the second one", "go to the first one",
        // "photo N" only on its own or after "go to".
        "pick, photo 12", "pick photo three", "photo five two",
        // Homophone counts (review-g L7).
        "next to", "back to", "skip for", "next won", "next too", "forward to", "skip to the next", "next for",
        "select 1 photo", "apply to 1 photo", "five photo",
        // A rating then a count or a place, a place then a rating.
        "two four", "one two three", "one second", "four five", "for the next two", "five next 3", "last five",
        "three stars for the next five", "rate five for the next two",
        // Verbs before counts the grammar can't read (review-g H1, M6).
        "reject the first 3", "pick the last two", "reject the previous 3", "pick the previous two",
        "pick the previous one", "pick next 3", "reject next two", "select next four", "pick the next to",
        "rate the next four", "rate next four three stars",
        // Undo with a place.
        "undo the last two", "undo the last one",
        // Fragments.
        "pick these", "these two", "these three photos", "two photos", "rate the next", "select", "apply to the",
        "number", "show", "go to", "jump to", "give me five", "back and forth", "twenty to five",
        // Everyday sentences.
        "for the next five minutes", "wait a second", "next three weeks", "I need help with this", "help me",
        "stop listening to me", "these three are great", "show me the money", "photo of the day", "we're number one",
        "undo the damage", "next to the door", "a hundred photos", "number one fan", "select the next week",
        "go to sleep", "I'm going back to the first place", "edited", "five o'clock", "red wine", "next week",
    ]

    @Test func table() {
        for (s, expected) in Self.cases { #expect(VoiceCommandParser.parse(s) == expected, "\(s)") }
    }

    @Test func roomTalkFiresNothing() {
        #expect(Self.roomTalk.count >= 40)
        for s in Self.roomTalk { #expect(VoiceCommandParser.parse(s) == nil, "\(s)") }
    }

    /// "three" alone is three stars; "next three" is a move of three.
    @Test func ratingsAndMovesStayApart() {
        #expect(VoiceCommandParser.parse("three") == [rate(3)])
        #expect(VoiceCommandParser.parse("next three") == [.step(3)])
        #expect(VoiceCommandParser.parse("three next") == [rate(3), next])
        #expect(VoiceCommandParser.parse("two four") == nil, "still not culling")
        #expect(VoiceCommandParser.parse("next three, four stars") == nil, "two ratings in a row are numbers in a sentence")
    }

    @Test func numbers() {
        func count(_ s: String) -> Int? { VoiceCommandParser.count(VoiceCommandParser.tokens(s), at: 0)?.value }
        func ordinal(_ s: String) -> Int? { VoiceCommandParser.ordinal(VoiceCommandParser.tokens(s), at: 0)?.value }
        #expect(count("seven") == 7 && count("12") == 12 && count("ninety nine") == 99 && count("forty") == 40)
        #expect(count("to") == 2 && count("for") == 4 && count("won") == 1 && count("fife") == 5)
        #expect(count("twenty to") == 20, "a homophone never compounds")
        #expect(count("999") == 999 && count("1000") == nil && count("9223372036854775807") == nil, "no overflow")
        #expect(count("seventh") == nil && count("") == nil)
        #expect(ordinal("fourth") == 4 && ordinal("4th") == 4 && ordinal("1st") == 1 && ordinal("33rd") == 33)
        #expect(ordinal("forty second") == 42 && ordinal("forth") == nil && ordinal("th") == nil && ordinal("four") == nil)
        #expect(ordinal("1000th") == nil)
        #expect(VoiceCommandParser.parse("next 1000") == nil && VoiceCommandParser.parse("undo 50 times") == nil)
    }

    @Test func settledPartials() {
        func settled(_ s: String) -> Bool? { VoiceCommandParser.analyze(s)?.settled }
        #expect(settled("next 3") == false, "may become next 30")
        #expect(settled("next three photos") == true)
        #expect(settled("reject the next 3") == false)
        #expect(settled("reject the next 3 photos") == true)
        #expect(settled("pick the") == false, "the speaker isn't done")
        #expect(VoiceCommandParser.parse("pick the") == [.key(.pick)])
        #expect(settled("photo 12") == false)
        #expect(settled("undo three times") == true)
        #expect(settled("pick these three photos") == false, "“these” waits for the final")
        #expect(settled("rate the next four three") == false && settled("rate the next four three stars") == true)
        #expect(settled("show picks") == true && settled("stop listening") == true)
    }

    /// What a push-to-talk release waits past (review-g H4).
    @Test func continuations() {
        func continues(_ s: String) -> Bool? { VoiceCommandParser.analyze(s)?.continues }
        for s in ["pick the", "pick", "reject", "five next", "pick next", "unflag the", "back", "five and", "undo"] {
            #expect(continues(s) == true, "\(s)")
        }
        for s in ["five", "pick the next two", "next 3", "zoom in", "five stars", "show picks"] {
            #expect(continues(s) == false, "\(s)")
        }
    }

    @Test func labels() {
        #expect(VoiceAction.step(3).label == "Forward 3" && VoiceAction.step(-2).label == "Back 2")
        #expect(VoiceAction.goTo(12).label == "Photo 12")
        #expect(VoiceAction.batch(.reject, nextN(3)).label == "Reject next 3")
        #expect(VoiceAction.batch(.rate(3), these(2)).label == "Rate ★★★ these 2")
        #expect(VoiceAction.batch(.rate(0), these(2)).label == "Rate no stars these 2")
        #expect(VoiceAction.batch(.pick, nextN(2)).label == "Pick next 2")
        #expect(VoiceAction.batch(.unflag, nextN(2)).label == "Unflag next 2")
        #expect(VoiceAction.batch(.select, nextN(2)).label == "Select next 2")
        #expect(VoiceAction.batch(.apply, nextN(2)).label == "Apply to next 2")
        #expect(VoiceAction.repeated(.undo, 3).label == "Undo ×3")
        #expect(VoiceAction.showFlags(.picked).label == "Show picks")
        #expect(VoiceAction.showFlags(.rejected).label == "Show rejects")
        #expect(VoiceAction.showFlags(.unflagged).label == "Show unflagged")
        #expect(VoiceAction.showFlags(.unrejected).label == "Show not rejected")
        #expect(VoiceAction.showFlags(.all).label == "Show all")
        #expect(VoiceAction.help(true).label == "Voice commands" && VoiceAction.help(false).label == "Close voice commands")
        #expect(VoiceAction.stopListening.label == "Stop listening")
        #expect(VoiceAction.key(.togglePinCompare).label == "Compare")
        #expect(VoiceAction.menu(.rotateLeft).label == "Rotate left" && VoiceAction.menu(.rotateRight).label == "Rotate right")
        #expect(seek(.picked, forward: false).label == "Previous pick")
        #expect(seek(.rejected).label == "Next reject" && seek(.unflagged).label == "Next unflagged")
        #expect(seek(.stars(5)).label == "Next ★★★★★")
    }

    @Test func undoableAndStableIDs() {
        for a in [rate(3), .key(.pick), .key(.tag(.red)), .batch(.reject, nextN(2)), .menu(.autoAdjust)] as [VoiceAction] {
            #expect(a.isUndoable, "\(a)")
        }
        for a in [next, .step(3), .goTo(4), .menu(.zoomIn), .batch(.select, nextN(2)), .help(true)] as [VoiceAction] {
            #expect(!a.isUndoable, "\(a)")
        }
        #expect(rate(5).stableID == "rate-5" && VoiceAction.key(.pick).stableID == "key-pick")
        #expect(VoiceAction.key(.tag(.red)).stableID == "tag-red" && next.stableID == "next" && previous.stableID == "previous")
        #expect(VoiceAction.menu(.zoomIn).stableID == "menu-zoomIn" && VoiceAction.jump(.last).stableID == "last")
        #expect(seek(.picked, forward: false).stableID == "previous-picked")
        #expect(VoiceAction.showFlags(.picked).stableID == "show-picked")
        #expect(VoiceAction.help(false).stableID == "close-help" && VoiceAction.stopListening.stableID == "stop-listening")
        #expect(VoiceAction.setPanelsHidden(true).stableID == "hide-panels" && VoiceAction.setPanelsHidden(false).stableID == "show-panels")
        #expect(VoiceAction.setFullScreen(false).stableID == "exit-full-screen" && VoiceAction.setFullScreen(true).stableID == "enter-full-screen")
        #expect(VoiceAction.step(2).stableID == nil && VoiceAction.edit("x").stableID == nil)
    }

    @Test func vocabularyHasTheNewWords() {
        let v = VoiceCommandParser.vocabulary
        for w in ["photo", "number", "these", "fourth", "show picks", "what can i say", "stop listening"] {
            #expect(v.contains(w), "\(w)")
        }
    }
}

// MARK: - The table and the Voice Commands sheet

@Suite struct VoiceCatalogTests {
    private var fixed: [VoiceCommand] { VoiceCommandParser.catalog.flatMap(\.commands).filter { $0.action != nil } }

    @Test func everyListedPhraseMeansItsCommand() {
        for c in fixed {
            for p in c.phrases + c.hidden { #expect(VoiceCommandParser.parse(p) == [c.action!], "\(p)") }
        }
    }

    /// Every example the sheet shows runs what its row says (no example is tested only by itself).
    @Test func everyExampleRunsWhatTheSheetSays() {
        for c in VoiceCommandParser.catalog.flatMap(\.commands) where c.action == nil {
            for p in c.phrases {
                let parsed = VoiceCommandParser.parse(p)
                #expect(parsed != nil && Self.examples[p] == parsed, "\(c.summary): \(p) → \(String(describing: parsed))")
                #expect(parsed.map { Self.fits(c.summary, $0) } == true, "\(c.summary): \(p)")
            }
        }
    }

    /// The kind of action each pattern row promises.
    private static func fits(_ summary: String, _ actions: [VoiceAction]) -> Bool {
        guard actions.count == 1, let a = actions.first else { return false }
        switch (summary, a) {
        case ("Move several photos", .step): return true
        case ("Go to a photo", .goTo): return true
        case ("Next or previous with that many stars", .jump(.seek(.stars, _))): return true
        case ("Flag several", .batch(.pick, _)), ("Flag several", .batch(.reject, _)), ("Flag several", .batch(.unflag, _)):
            return true
        case ("Rate several", .batch(.rate, _)), ("Select several", .batch(.select, _)),
             ("Apply copied adjustments", .batch(.apply, _)), ("Undo several", .repeated), ("Edit with words", .edit):
            return true
        default: return false
        }
    }

    /// What each example in the sheet runs (a new example needs a line here).
    static let examples: [String: [VoiceAction]] = Dictionary(
        VoiceLanguageParserTests.cases.map { ($0.0, $0.1) } + [
            ("previous 3 star", [seek(.stars(3), forward: false)]),
            ("edit warmer and a bit brighter", [.edit("warmer and a bit brighter")]),
        ]) { a, _ in a }

    /// Each note's claim, checked by the parser.
    @Test func notesAreTrue() {
        func p(_ s: String) -> [VoiceAction]? { VoiceCommandParser.parse(s) }
        #expect(p("next three stars") == [next, rate(3)])
        #expect(p("next to") == nil && p("skip for") == nil)
        #expect(p("the second one") == nil && p("number one") == nil)
        #expect(p("forward to the fourth") == [.goTo(4)] && p("forward four") == [.step(4)])
        #expect(p("next five stars") == [next, rate(5)] && p("next 1 star") == [seek(.stars(1))])
        #expect(p("reject the next 3") == [.batch(.reject, nextN(3))] && p("reject next 3 photos") == [.batch(.reject, nextN(3))])
        #expect(p("reject these 3") == [.batch(.reject, these(3))] && p("reject 3 photos") == [.batch(.reject, these(3))])
        #expect(p("pick next one") == [.key(.pick), next])
        #expect(p("undo five") == [undo, rate(5)])
        #expect(p("five next") == [rate(5), next] && p("pick next") == [.key(.pick), next], "the intro's chains")
        let notes = VoiceCommandParser.catalog.flatMap(\.commands).compactMap(\.note).joined(separator: " ")
        for claim in ["next three stars", "next to", "the second one", "number one", "forward four", "next five stars",
                      "next 1 star", "pick next one", "undo five", "never onto the copied photo"] {
            #expect(notes.contains(claim), "\(claim)")
        }
    }

    @Test func idsAreUniqueAndTablePhrasesAreAllListed() {
        let ids = fixed.map(\.id)
        #expect(Set(ids).count == ids.count)
        #expect(VoiceCommandParser.commandsByID.count == ids.count)
        #expect(VoiceCommandParser.commandsByID["key-pick"]?.summary == "Pick")
        let listed = Set(fixed.flatMap { ($0.phrases + $0.hidden).map { VoiceCommandParser.normalize($0) } })
        for key in VoiceCommandParser.phrases.keys where !listed.contains(key) {
            // The generated "next five star" family (its pattern row shows it) and keys whose
            // separators were dropped ("first photo" → "first").
            #expect(key.hasSuffix(" star") || listed.contains { $0.hasPrefix(key + " ") }, "\(key)")
        }
    }

    @Test func helpSheetListsTheCatalog() {
        let sections = VoiceHelp.sections()
        #expect(sections.map(\.title) == VoiceCommandParser.catalog.map(\.title))
        for (s, g) in zip(sections, VoiceCommandParser.catalog) {
            #expect(s.rows.map(\.summary) == g.commands.map(\.summary))
            #expect(s.rows.map(\.phrases) == g.commands.map { $0.phrases.map { "“\($0)”" } })
        }
        let all = sections.flatMap(\.rows).flatMap(\.phrases)
        for p in ["“next 3”", "“go to the 4th photo”", "“reject the next 3”", "“what can I say”", "“show picks”",
                  "“stop listening”", "“undo 3 times”", "“compare”", "“rotate left”"] {
            #expect(all.contains(p), "\(p)")
        }
        #expect(!all.contains("“necks”"), "mishearings aren't listed")
        #expect(!all.contains("“three stars for the next 5”"), "the rating-first form is gone")
        #expect(!VoiceHelp.intro.isEmpty)
        let ids = sections.flatMap(\.rows).map(\.id)
        #expect(Set(ids).count == ids.count)
    }

    @Test func helpSheetListsYourPhrases() {
        var book = VoicePhraseBook()
        #expect(book.add("keeper", for: "key-pick") == nil)
        let mine = VoiceHelp.sections(custom: book).last
        #expect(mine?.title == "Your phrases")
        #expect(mine?.rows == [VoiceHelp.Row(summary: "Pick", phrases: ["“keeper”"])])
    }

    @Test func cheatSheetPointsAtTheSheet() {
        let rows = KeyboardRouter.cheatSheet().first { $0.section == "Voice" }?.rows ?? []
        #expect(rows.contains { $0.keys == "“next 3” “go to 12”" })
        #expect(rows.contains { $0.keys == "“reject the next 3”" })
        #expect(rows.contains { $0.keys == "“what can I say?”" })
        #expect(VoiceText.help(.off).contains("what can I say"))
        #expect(VoiceText.languageNote.contains("what can I say"))
    }
}

// MARK: - Custom phrases

@MainActor
@Suite struct VoicePhraseBookTests {
    @Test func addAndHear() {
        var book = VoicePhraseBook()
        #expect(book.add("Keeper!", for: "key-pick") == nil)
        #expect(book.add("bin it", for: "key-reject") == nil)
        #expect(book.phrases == ["Keeper": "key-pick", "bin it": "key-reject"], "kept as typed")
        let custom = book.actions
        #expect(custom == ["keeper": .key(.pick), "bin": .key(.reject)], "matched by normalized words")
        #expect(VoiceCommandParser.parse("keeper", custom: custom) == [.key(.pick)])
        #expect(VoiceCommandParser.parse("bin it", custom: custom) == [.key(.reject)])
        #expect(VoiceCommandParser.parse("keeper next", custom: custom) == [.key(.pick), next], "chains like built-ins")
        #expect(VoiceCommandParser.parse("keeper", custom: [:]) == nil)
        #expect(VoiceCommandParser.parse("keeper of the keys", custom: custom) == nil, "room talk still fires nothing")
        #expect(book.rows.map(\.phrase) == ["Keeper", "bin it"])
        #expect(book.rows.map(\.command.summary) == ["Pick", "Reject"])
    }

    @Test func multiWordPhrasesWaitForTheirEnd() {
        var book = VoicePhraseBook()
        #expect(book.add("keep that one", for: "rate-5") == nil)
        let custom = book.actions
        #expect(VoiceCommandParser.parse("keep that one", custom: custom) == [rate(5)])
        #expect(VoiceCommandParser.parse("keep that", custom: custom) == nil)
        #expect(VoiceCommandParser.analyze("keep that one", custom: custom)?.settled == true)
        var shorter = book
        #expect(shorter.add("keep", for: "key-pick") == nil)
        #expect(VoiceCommandParser.analyze("keep", custom: shorter.actions)?.settled == false, "may become “keep that one”")
    }

    @Test func conflictsAreRefusedWithTheReason() {
        var book = VoicePhraseBook()
        #expect(book.refusal("next", for: "key-pick") == "“next” already means Next.")
        #expect(book.refusal("five next", for: "key-pick") == "“five next” already means ★★★★★ · Next.")
        #expect(book.refusal("next 3", for: "key-pick")?.contains("Forward 3") == true)
        #expect(book.refusal("won", for: "key-pick")?.contains("already means") == true, "what recognizers hear counts too")
        #expect(book.refusal("the", for: "key-pick")?.contains("no words") == true)
        #expect(book.refusal("  ", for: "key-pick")?.contains("no words") == true)
        #expect(book.refusal("edit warmer", for: "key-pick")?.contains("Edit with Words") == true)
        #expect(book.refusal("best photo 1", for: "key-pick")?.contains("position") == true)
        #expect(book.refusal("go to 3", for: "key-pick")?.contains("position") == true)
        #expect(book.refusal("keeper", for: "nonsense") == "Choose a command for it.")
        // review-g2 L4': phrases that start with a word voice skips would never fire.
        for phrase in ["picture perfect", "photo finish", "number cruncher", "the next keeper"] {
            #expect(book.refusal(phrase, for: "key-pick")?.contains("can’t be heard") == true, "\(phrase)")
        }
        var shot = book
        #expect(shot.add("nice shot", for: "key-pick") == nil)
        #expect(VoiceCommandParser.parse("nice shot", custom: shot.actions) == [.key(.pick)])
        #expect(book.add("keeper", for: "key-pick") == nil)
        #expect(book.add("keeper", for: "key-pick") == "“keeper” is already your phrase for Pick.")
        #expect(book.add("the keeper", for: "key-reject") == "“keeper” is already your phrase for Pick; remove it first.")
        #expect(book.add("keeper next", for: "key-reject") == "“keeper next” already means Pick · Next.",
                "your own phrases count too (review-g L6)")
        #expect(book.phrases == ["keeper": "key-pick"])
        book.remove("Keeper!")
        #expect(book.phrases.isEmpty)
    }

    /// Stored phrases are checked again on load: a gone command, or a phrase a newer built-in uses, is dropped.
    @Test func storedPhrasesAreCheckedAgain() {
        let book = VoicePhraseBook(["keeper": "key-pick", "x": "no such command", "compare": "key-reject",
                                    "keeper next": "key-reject"])
        #expect(book.phrases == ["keeper": "key-pick"])
    }

    @Test func targetsAreTheFixedCommands() {
        let targets = VoicePhraseBook.targets.flatMap(\.commands)
        #expect(targets.allSatisfy { $0.action != nil })
        #expect(targets.contains { $0.id == "key-pick" } && targets.contains { $0.id == "key-reject" })
        #expect(!VoicePhraseBook.targets.contains { $0.title == "Several photos" }, "patterns aren't targets")
    }

    @Test func persistInTheScratchPrefsAndReachTheController() {
        let s = makeSettings()
        let voice = VoiceController()
        #expect(s.voicePhrases.isEmpty)
        #expect(s.addVoicePhrase("keeper", for: "key-pick", voice: voice) == nil)
        #expect(s.addVoicePhrase("next", for: "key-pick", voice: voice) != nil)
        #expect(voice.customPhrases == ["keeper": .key(.pick)])
        let again = AppSettings(defaults: s.userDefaults)
        #expect(again.voicePhrases == ["keeper": "key-pick"])
        let m = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                         renderer: FakeRenderer(), importer: MockImportService(), archive: MockArchiveService(target: ""),
                         ai: MockAIService(), settings: again)
        #expect(m.voice.customPhrases == ["keeper": .key(.pick)])
        s.removeVoicePhrase("keeper", voice: voice)
        #expect(voice.customPhrases.isEmpty && AppSettings(defaults: s.userDefaults).voicePhrases.isEmpty)
    }

    @Test func listeningRunHearsCustomPhrases() async {
        let m = makeModel()
        await m.start()
        let id = m.focusedID!
        let engine = FakeVoiceEngine()
        m.voice.engine = engine
        m.voice.permissions = FakeVoicePermissions()
        #expect(m.settings.addVoicePhrase("bin it", for: "key-reject", voice: m.voice) == nil)
        await m.voice.turnOn()
        #expect(await waitUntil { m.voice.status == .listening })
        #expect(engine.vocabulary.contains("bin"))
        #expect(m.settings.addVoicePhrase("keeper", for: "key-pick", voice: m.voice) == nil)
        engine.send(.transcript(VoiceTranscript("keeper", isFinal: true)))
        #expect(await waitUntil { m.photo(id)?.pick == .picked }, "added while listening: heard at once")
        engine.send(.transcript(VoiceTranscript("bin it", isFinal: true)))
        #expect(await waitUntil { m.photo(id)?.pick == .rejected })
        m.voice.turnOff()
        m.stop()
    }
}

// MARK: - The tracker

@Suite struct VoiceLanguageTrackerTests {
    private func t(_ s: String, final: Bool = false) -> VoiceTranscript { VoiceTranscript(s, isFinal: final, confidence: 0.9) }

    /// "pick" settles, but "pick the" says more is coming: nothing fires before "pick the next two".
    @Test func goingOnCancelsTheEarlyFire() {
        var tr = VoiceUtteranceTracker(mode: .pushToTalk, settle: 0.2)
        _ = tr.receive(t("pick"), at: 0)
        #expect(tr.deadline == 0.2)
        _ = tr.receive(t("pick the"), at: 0.1)
        #expect(tr.deadline == nil)
        #expect(tr.tick(at: 1).isEmpty)
        _ = tr.receive(t("pick the next two"), at: 1.1)
        #expect(tr.tick(at: 2).isEmpty, "a count at the end may still grow")
        #expect(tr.receive(t("pick the next two", final: true), at: 2.1) == [.batch(.pick, nextN(2))])
    }

    /// review-g H4 / review-g2 H4': a key release on a partial that isn't settled (or leads
    /// into more) fires nothing; the final then fires the whole command, once.
    @Test func releaseOnAContinuationWaitsForTheFinal() {
        let pairs: [(partial: String, final: String, runs: [VoiceAction])] = [
            ("pick the", "pick the next two", [.batch(.pick, nextN(2))]),
            ("pick the next", "pick the next two", [.batch(.pick, nextN(2))]),
            ("reject", "reject the next 3", [.batch(.reject, nextN(3))]),
            ("reject three", "reject three photos", [.batch(.reject, these(3))]),
            ("pick 2", "pick 2 photos", [.batch(.pick, these(2))]),
            ("undo three", "undo three times", [.repeated(.undo, 3)]),
            ("redo two", "redo two steps", [.repeated(.redo, 2)]),
            ("rate 3", "rate 3 photos five stars", [.batch(.rate(5), these(3))]),
            ("next 3", "next three photos", [.step(3)]),
            ("five", "five stars", [rate(5)]),
            ("five and", "five and next", [rate(5), next]),
        ]
        for p in pairs {
            var tr = VoiceUtteranceTracker(mode: .pushToTalk, settle: 0.2)
            _ = tr.receive(t(p.partial), at: 0)
            #expect(tr.flush().isEmpty, "\(p.partial)")
            #expect(tr.pending == nil)
            #expect(tr.receive(t(p.final, final: true), at: 0.5) == p.runs, "\(p.final)")
            #expect(tr.takeContradiction() == nil, "\(p.final)")
        }
        // A settled partial still fires at release, and the final adds only the rest.
        var tr = VoiceUtteranceTracker(mode: .pushToTalk, settle: 0.2)
        _ = tr.receive(t("five stars"), at: 0)
        #expect(tr.flush() == [rate(5)])
        #expect(tr.receive(t("five stars next", final: true), at: 0.5) == [next])
    }

    /// review-g2 N1: what a release held back is remembered until its final arrives.
    @Test func heldBackIsRememberedUntilTheFinal() {
        var tr = VoiceUtteranceTracker(mode: .pushToTalk, settle: 0.2)
        _ = tr.receive(t("next 3"), at: 0)
        #expect(tr.flush().isEmpty)
        #expect(tr.heldBack == [.step(3)])
        #expect(tr.takeHeldBack() == [.step(3)])
        #expect(tr.takeHeldBack() == nil, "taken once")
        _ = tr.receive(t("reject three"), at: 1)
        _ = tr.flush()
        #expect(tr.heldBack != nil)
        #expect(tr.receive(t("reject three photos", final: true), at: 1.5) == [.batch(.reject, these(3))])
        #expect(tr.heldBack == nil, "the final came: nothing is lost")
        _ = tr.receive(t("five stars"), at: 2)
        #expect(tr.flush() == [rate(5)])
        #expect(tr.heldBack == nil, "a settled partial fires; nothing is held back")
        _ = tr.receive(t("pick"), at: 3)
        _ = tr.flush()
        tr.reset()
        #expect(tr.heldBack == nil)
    }

    /// review-g2 FU1: a late settled partial after the release runs the command; nothing
    /// is held back any more, so no "say it again".
    @Test func lateSettledPartialClearsWhatWasHeldBack() {
        var tr = VoiceUtteranceTracker(mode: .pushToTalk, settle: 0.2)
        _ = tr.receive(t("reject three"), at: 0)
        #expect(tr.flush().isEmpty && tr.heldBack != nil)
        _ = tr.receive(t("reject three photos"), at: 0.5)
        #expect(tr.flush() == [.batch(.reject, these(3))])
        #expect(tr.takeHeldBack() == nil)
    }

    /// review-g2 FU2: continuous listening never holds back (only a push-to-talk release does).
    @Test func continuousNeverHoldsBack() {
        var tr = VoiceUtteranceTracker(mode: .continuous, settle: 0.2)
        _ = tr.receive(t("five"), at: 0)
        #expect(tr.flush().isEmpty)
        #expect(tr.heldBack == nil)
    }

    @Test func contradictionSaysWhetherUndoHelps() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        _ = tr.receive(t("zoom in"), at: 0)
        #expect(tr.tick(at: 1) == [.menu(.zoomIn)])
        _ = tr.receive(t("zoom in on her", final: true), at: 2)
        #expect(tr.takeContradiction() != nil && !tr.contradictionUndoable)
        _ = tr.receive(t("five stars"), at: 3)
        #expect(tr.tick(at: 4) == [rate(5)])
        _ = tr.receive(t("five stars for her", final: true), at: 5)
        #expect(tr.takeContradiction() != nil && tr.contradictionUndoable)
    }

    @Test func customPhrasesFire() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        tr.custom = ["keeper": .key(.pick)]
        #expect(tr.receive(t("keeper", final: true), at: 0) == [.key(.pick)])
    }
}

// MARK: - On the app

/// Serialized: each test starts a whole app model; run side by side they crowd the
/// machine's other timing-sensitive suites.
@MainActor
@Suite(.serialized) struct VoiceLanguageAppTests {
    private func started(autoAdvance: Bool = false) async -> AppModel {
        let m = makeModel()
        await m.start()
        m.settings.autoAdvance = autoAdvance
        return m
    }

    @Test func stepsAndPositions() async {
        let m = await started()
        let ids = m.visibleIDs
        #expect(m.performVoice([.step(3)]) == "Forward 3")
        #expect(m.focusedID == ids[3])
        #expect(m.performVoice([.step(-2)]) == "Back 2")
        #expect(m.focusedID == ids[1])
        #expect(m.performVoice([.goTo(12)]) == "Photo 12")
        #expect(m.focusedID == ids[11])
        #expect(m.performVoice([.goTo(999)]) == nil)
        #expect(m.toast?.message == "There is no photo 999: \(ids.count) are shown")
        #expect(m.focusedID == ids[11])
        m.performVoice([.step(500)])
        #expect(m.focusedID == ids.last, "stops at the end")
        #expect(m.performVoice([.step(3)]) == nil, "nothing to move to")
        #expect(m.toast?.message == "Already at the last photo")
        m.performVoice([.step(-500)])
        #expect(m.focusedID == ids[0])
        #expect(m.performVoice([.step(-2)]) == nil)
        #expect(m.toast?.message == "Already at the first photo")
        #expect(m.performVoice(VoiceCommandParser.parse("go forward to the fourth")!) == "Photo 4")
        #expect(m.focusedID == ids[3])
        m.stop()
    }

    @Test func stepsWithNothingFocused() async {
        let m = await started()
        let ids = m.visibleIDs
        m.focus(nil, extend: false)
        m.performVoice([.step(2)])
        #expect(m.focusedID == ids[1], "from before the first")
        m.focus(nil, extend: false)
        m.performVoice([.step(-2)])
        #expect(m.focusedID == ids[ids.count - 2], "from after the last")
        m.stop()
    }

    @Test func batchIsOneUndoStepAndSaysWhatItDid() async {
        let m = await started()
        let ids = m.visibleIDs
        m.updatePhotos(ids, label: "t") { $0.pick = .none; $0.rating = 0 }
        #expect(m.performVoice(VoiceCommandParser.parse("reject the next 3")!) == "Rejected 3 photos")
        #expect(m.focusedID == ids[0], "focus stays")
        #expect(m.photo(ids[0])?.pick == PickState.none, "“the next 3” are after this photo")
        #expect(ids[1...3].allSatisfy { m.photo($0)?.pick == .rejected })
        #expect(m.photo(ids[4])?.pick == PickState.none)
        #expect(m.history.undoLabel == "Reject 3 photos")
        m.performVoice([.key(.undo)])
        #expect(ids[1...3].allSatisfy { m.photo($0)?.pick == PickState.none }, "one undo takes back all three")
        #expect(m.performVoice(VoiceCommandParser.parse("pick these three")!) == "Picked 3 photos")
        #expect(ids[0...2].allSatisfy { m.photo($0)?.pick == .picked })
        #expect(m.performVoice([.batch(.unflag, these(2))]) == "Unflagged 2 photos")
        #expect(m.photo(ids[0])?.pick == PickState.none && m.photo(ids[2])?.pick == .picked)
        m.stop()
    }

    /// review-g H1: "reject, next one" rejects this photo and moves on, as on main.
    @Test func oneMoreStaysTheOldChain() async {
        let m = await started()
        let ids = m.visibleIDs
        m.updatePhotos(ids, label: "t") { $0.pick = .none }
        m.performVoice(VoiceCommandParser.parse("Reject. Next one.")!)
        #expect(m.photo(ids[0])?.pick == .rejected && m.photo(ids[1])?.pick == PickState.none)
        #expect(m.focusedID == ids[1])
        m.stop()
    }

    /// review-g H2: "undo, five" is one undo and a rating, never five undos.
    @Test func undoFiveIsUndoThenFiveStars() async {
        let m = await started()
        let ids = m.visibleIDs
        m.updatePhotos([ids[0]], label: "t") { $0.rating = 0 }
        m.updatePhotos([ids[0]], label: "t") { $0.pick = .picked }
        m.performVoice([rate(2)])
        m.performVoice(VoiceCommandParser.parse("undo, five")!)
        #expect(m.photo(ids[0])?.rating == 5 && m.photo(ids[0])?.pick == .picked, "the earlier pick survives")
        m.stop()
    }

    @Test func batchRatings() async {
        let m = await started()
        let ids = m.visibleIDs
        #expect(m.performVoice(VoiceCommandParser.parse("rate the next four three stars")!) == "Rated 4 photos ★★★")
        #expect(ids[1...4].allSatisfy { m.photo($0)?.rating == 3 })
        #expect(m.history.undoLabel == "Rate 4 photos 3")
        m.focus(ids[1], extend: false)
        #expect(m.performVoice([.batch(.rate(0), these(2))]) == "Cleared the rating of 2 photos")
        #expect(m.photo(ids[1])?.rating == 0 && m.photo(ids[2])?.rating == 0)
        #expect(m.history.undoLabel == "Clear Rating of 2 photos")
        m.stop()
    }

    @Test func batchStopsAtTheEndAndNeedsAVisibleFocus() async {
        let m = await started()
        let ids = m.visibleIDs
        m.focus(ids[ids.count - 2], extend: false)
        #expect(m.performVoice([.batch(.reject, nextN(3))]) == "Rejected 1 photo")
        m.focus(ids.last, extend: false)
        #expect(m.performVoice([.batch(.reject, nextN(3))]) == nil)
        #expect(m.toast?.message == "No photos after this one")
        m.focus(nil, extend: false)
        #expect(m.performVoice([.batch(.reject, nextN(3))]) == nil)
        #expect(m.toast?.message == "No photo is focused")
        // review-g L5: a filter that hides the focused photo never shifts the start.
        m.updatePhotos(ids, label: "t") { $0.pick = .none }
        m.updatePhotos([ids[4], ids[5], ids[6]], label: "t") { $0.pick = .picked }
        m.filter.pick = .picked
        m.focus(ids[0], extend: false) // a hidden photo focused (the filter itself would refocus)
        #expect(m.focusedID == ids[0] && m.visibleIndex(of: ids[0]) == nil)
        #expect(m.performVoice([.batch(.reject, these(2))]) == nil)
        #expect(m.toast?.message == "The focused photo is hidden by the filter")
        #expect(m.photo(ids[4])?.pick == .picked)
        m.stop()
    }

    @Test func selectSelectsExactlyTheSpanAndFocusesItsFirst() async {
        let m = await started()
        let ids = m.visibleIDs
        m.updatePhotos(ids, label: "t") { $0.pick = .none }
        m.selection = [ids[20]] // an older selection elsewhere is replaced, not added to
        #expect(m.performVoice(VoiceCommandParser.parse("select the next 3")!) == "Selected 3 photos")
        #expect(m.selection == Set(ids[1...3]), "exactly the photos named, like “reject the next 3” (review-g2 L2')")
        #expect(m.focusedID == ids[1], "focus moves onto the first selected photo (review-g2 N3)")
        m.performVoice([.key(.pick)])
        #expect(ids[1...3].allSatisfy { m.photo($0)?.pick == .picked }, "a plain pick acts on what is on screen")
        #expect(m.photo(ids[0])?.pick == PickState.none && m.photo(ids[20])?.pick == PickState.none)
        #expect(m.performVoice([.batch(.select, these(2))]) == "Selected 2 photos")
        #expect(m.selection == Set(ids[1...2]) && m.focusedID == ids[1])
        m.performVoice([.key(.move(.right, extend: true))])
        #expect(m.selection == Set(ids[1...2]), "the anchor is the first selected photo: ⇧→ extends from it")
        m.stop()
    }

    /// review-g M4: Apply pastes what Copy's options say (Edited: only the changed groups), never onto the source.
    @Test func applyToSeveralUsesCopysOptions() async {
        let m = await started()
        let ids = m.visibleIDs
        #expect(m.performVoice(VoiceCommandParser.parse("apply to the next 3")!) == nil, "nothing copied")
        #expect(m.toast?.message == "Copy adjustments first (“copy settings”)")
        m.focus(ids[1], extend: false)
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        m.focus(ids[0], extend: false)
        m.updateEdit(label: "White balance") { $0.whiteBalance.asShot = false; $0.whiteBalance.temperature = 3000 }
        #expect(m.settings.copyMode == .edited)
        m.performVoice([.menu(.copyAdjustments)])
        #expect(m.clipboard?.sourceID == ids[0])
        #expect(m.performVoice(VoiceCommandParser.parse("apply to these 3")!) == "Applied adjustments to 2 photos",
                "never onto the copied photo")
        #expect(await waitUntil { ids[1...2].allSatisfy { m.edit(for: $0).whiteBalance.temperature == 3000 } })
        #expect(m.edit(for: ids[1]).exposure.ev == 1, "only the copied groups are pasted")
        #expect(m.edit(for: ids[3]).whiteBalance.temperature != 3000)
        #expect(m.history.undoLabel == "Apply Adjustments", "one undo step")
        m.performVoice([.key(.undo)])
        #expect(ids[1...2].allSatisfy { m.edit(for: $0).whiteBalance.temperature != 3000 })
        m.focus(ids[0], extend: false)
        #expect(m.performVoice([.batch(.apply, these(1))]) == nil)
        #expect(m.toast?.message == "Apply never pastes onto the copied photo")
        m.settings.copyMode = .custom
        m.settings.copyGroups = []
        #expect(m.performVoice([.batch(.apply, nextN(2))]) == nil)
        #expect(m.toast?.message == "No adjustments to apply: choose them in Copy’s options")
        m.stop()
    }

    @Test func seeks() async {
        let m = await started()
        let ids = m.visibleIDs
        m.updatePhotos(ids, label: "t") { $0.rating = 2; $0.pick = .picked }
        m.updatePhotos([ids[2]], label: "t") { $0.pick = .rejected }
        m.updatePhotos([ids[4]], label: "t") { $0.pick = .none }
        m.updatePhotos([ids[6]], label: "t") { $0.rating = 5 }
        #expect(m.performVoice(VoiceCommandParser.parse("next reject")!) == "Next reject")
        #expect(m.focusedID == ids[2])
        m.performVoice(VoiceCommandParser.parse("next unflagged")!)
        #expect(m.focusedID == ids[4])
        m.performVoice(VoiceCommandParser.parse("next five star")!)
        #expect(m.focusedID == ids[6])
        m.performVoice(VoiceCommandParser.parse("previous reject")!)
        #expect(m.focusedID == ids[2])
        m.performVoice(VoiceCommandParser.parse("previous pick")!)
        #expect(m.focusedID == ids[1])
        m.performVoice(VoiceCommandParser.parse("previous pick")!)
        #expect(m.focusedID == ids[0])
        #expect(m.performVoice(VoiceCommandParser.parse("previous pick")!) == nil)
        #expect(m.toast?.message == "No earlier picked photos")
        #expect(m.performVoice(VoiceCommandParser.parse("previous unrated")!) == nil)
        #expect(m.performVoice([seek(.stars(5), forward: false)]) == nil)
        #expect(m.toast?.message == "No earlier 5-star photos")
        m.stop()
    }

    @Test func filterBar() async {
        let m = await started()
        let ids = m.visibleIDs
        m.updatePhotos(ids, label: "t") { $0.pick = .none }
        m.updatePhotos([ids[1], ids[3]], label: "t") { $0.pick = .picked }
        #expect(m.performVoice([.showFlags(.picked)]) == "Show picks")
        #expect(m.filter.pick == .picked && m.visibleIDs == [ids[1], ids[3]])
        m.performVoice(VoiceCommandParser.parse("show all")!)
        #expect(m.filter.pick == .all && m.visibleIDs == ids)
        m.stop()
    }

    @Test func undoSeveralCompareAndRotate() async {
        let m = await started()
        let id = m.focusedID!
        m.updatePhotos([id], label: "t") { $0.rating = 0 }
        let before = m.photo(id)?.rating
        for r in [1, 2, 3] { m.performVoice([rate(r)]) }
        #expect(m.performVoice(VoiceCommandParser.parse("undo 3 times")!) == "Undo ×3")
        #expect(m.photo(id)?.rating == before)
        #expect(m.performVoice(VoiceCommandParser.parse("compare")!) == "Compare")
        #expect(m.viewer.state.comparePinned == id)
        #expect(m.performVoice(VoiceCommandParser.parse("rotate right")!) == "Rotate right")
        m.stop()
    }

    @Test func helpSheetFromVoiceMenuAndEsc() async {
        let m = await started()
        #expect(m.performVoice(VoiceCommandParser.parse("what can I say")!) == "Voice commands")
        #expect(m.voice.helpShown)
        #expect(m.performVoice([next]) == "Next", "voice still works with the sheet up")
        m.performVoice(VoiceCommandParser.parse("close help")!)
        #expect(!m.voice.helpShown)
        #expect(CommandCatalog.title(.voiceHelp, model: m) == "What Can I Say?")
        #expect(CommandCatalog.isEnabled(.voiceHelp, model: m) && MenuCommand.voiceHelp.shortcut == nil)
        CommandCatalog.perform(.voiceHelp, model: m)
        #expect(m.voice.helpShown)
        m.perform(.escape)
        #expect(!m.voice.helpShown, "Esc closes it")
        m.stop()
    }

    @Test func stopListening() async {
        let m = await started()
        let engine = FakeVoiceEngine()
        m.voice.engine = engine
        m.voice.permissions = FakeVoicePermissions()
        await m.voice.turnOn()
        #expect(await waitUntil { m.voice.status == .listening })
        engine.send(.transcript(VoiceTranscript("stop listening", isFinal: true)))
        #expect(await waitUntil { !m.voice.isOn })
        #expect(await waitUntil { engine.stops == [1] }, "the microphone is released")
        m.stop()
    }

    /// With auto-advance, "five, next 3" moves three in all (the rating's advance is one of
    /// them), and echoes what was said.
    @Test func chainCountsTheAutoAdvance() async {
        let m = await started(autoAdvance: true)
        let ids = m.visibleIDs
        #expect(m.performVoice([rate(5), .step(3)]) == "★★★★★ · Forward 3")
        #expect(m.focusedID == ids[3])
        m.performVoice([rate(4), .step(2)])
        #expect(m.focusedID == ids[5])
        m.stop()
    }

    /// review-g M3: with auto-advance, "pick, reject the next 3" rejects the three after
    /// the picked photo (the pick's advance counts). A rating before a count is refused,
    /// so a flag is the chain that reaches this (review-g2 L3').
    @Test func chainBatchCountsFromTheFlaggedPhoto() async {
        let m = await started(autoAdvance: true)
        let ids = m.visibleIDs
        m.updatePhotos(ids, label: "t") { $0.pick = .none }
        #expect(VoiceCommandParser.parse("five, reject the next 3") == nil)
        m.performVoice(VoiceCommandParser.parse("pick, reject the next 3")!)
        #expect(m.photo(ids[0])?.pick == .picked)
        #expect(ids[1...3].allSatisfy { m.photo($0)?.pick == .rejected })
        #expect(m.photo(ids[4])?.pick == PickState.none)
        m.stop()
    }

    /// review-g2 N1: a release held "next" back for the final, and the stream ended without
    /// one (the recognizer gave up): nothing runs, and the HUD says so.
    @Test func pushToTalkWithNoFinalSaysSoAndRunsNothing() async {
        let engine = FakeVoiceEngine()
        let v = VoiceController(engine: engine, permissions: FakeVoicePermissions())
        var ran: [[VoiceAction]] = []
        v.perform = { actions, _ in
            ran.append(actions)
            return actions.map(\.label).joined(separator: " · ")
        }
        v.now = { 0 }
        v.echoDuration = 60
        v.pushToTalk = true
        await v.turnOn()
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening })
        engine.send(.transcript(VoiceTranscript("next 3")))
        #expect(await waitUntil { v.tracker.pending != nil })
        #expect(v.handlePushToTalk(code: 61, pressed: false)) // finish ends the stream: no final
        #expect(await waitUntil { v.echo?.text == VoiceText.notCaught })
        #expect(ran.isEmpty, "never guessed and run (review-g2 H4')")
        // With a final, nothing is lost and nothing is said.
        engine.holdsFinishing = true
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening && engine.session == 2 })
        engine.send(.transcript(VoiceTranscript("next 3")))
        #expect(await waitUntil { v.tracker.pending != nil })
        #expect(v.handlePushToTalk(code: 61, pressed: false))
        #expect(await waitUntil { engine.finishes == [1, 2] })
        engine.deliverLateFinal(VoiceTranscript("next 3", isFinal: true), session: 2)
        #expect(await waitUntil { ran == [[.step(3)]] })
        #expect(v.echo?.text == "Forward 3")
        v.turnOff()
    }

    /// review-g2 FU2: in continuous mode, a stream that ends by itself (the audio input
    /// changed) with room talk pending shows no push-to-talk note and runs nothing.
    @Test func continuousStreamEndSaysNothing() async {
        let engine = FakeVoiceEngine()
        let v = VoiceController(engine: engine, permissions: FakeVoicePermissions())
        var ran: [[VoiceAction]] = []
        v.perform = { actions, _ in
            ran.append(actions)
            return actions.map(\.label).joined(separator: " · ")
        }
        v.now = { 0 }
        v.echoDuration = 60
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening && engine.session == 1 })
        engine.send(.transcript(VoiceTranscript("five")))
        #expect(await waitUntil { v.tracker.pending != nil })
        engine.endByItself()
        #expect(await waitUntil { engine.session == 2 }, "a new run starts")
        #expect(v.echo == nil && ran.isEmpty)
        v.turnOff()
    }

    /// review-g H4 through the real key-release path: holding the key, "pick the" is all the
    /// recognizer had at release; nothing runs until the final, which runs only the batch.
    @Test func pushToTalkReleaseWaitsForTheBatch() async {
        let m = await started()
        let ids = m.visibleIDs
        m.updatePhotos(ids, label: "t") { $0.pick = .none }
        let engine = FakeVoiceEngine()
        m.voice.engine = engine
        m.voice.permissions = FakeVoicePermissions()
        m.voice.sleep = { _ in }
        m.voice.now = { 0 }
        let perform = m.voice.perform
        var performed: [[VoiceAction]] = []
        m.voice.perform = { actions, chain in
            performed.append(actions)
            return perform(actions, &chain)
        }
        m.voice.pushToTalk = true
        await m.voice.turnOn()
        #expect(m.voice.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { m.voice.status == .listening })
        engine.send(.transcript(VoiceTranscript("pick the")))
        #expect(await waitUntil { m.voice.tracker.pending != nil })
        engine.holdsFinishing = true
        #expect(m.voice.handlePushToTalk(code: 61, pressed: false))
        #expect(performed.isEmpty, "the release waits")
        #expect(m.photo(ids[0])?.pick == PickState.none)
        #expect(await waitUntil { engine.finishes == [1] })
        engine.deliverLateFinal(VoiceTranscript("pick the next two", isFinal: true), session: 1)
        #expect(await waitUntil { performed.count == 1 })
        #expect(performed == [[.batch(.pick, nextN(2))]])
        #expect(m.photo(ids[0])?.pick == PickState.none && ids[1...2].allSatisfy { m.photo($0)?.pick == .picked })
        m.voice.turnOff()
        m.stop()
    }
}
