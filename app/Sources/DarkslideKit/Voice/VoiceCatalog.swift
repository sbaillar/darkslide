import Foundation

/// One row of the Voice Commands sheet: what it does and how to say it.
public struct VoiceCommand: Identifiable, Sendable {
    public let summary: String
    /// Shown in the sheet, and heard.
    public let phrases: [String]
    /// Also heard, not shown: variants and what recognizers mishear.
    let hidden: [String]
    /// What it runs; `nil`: a pattern the grammar parses, and `phrases` are examples.
    public let action: VoiceAction?
    public let note: String?

    /// Stable across catalog edits (custom phrases are stored by it): the action's
    /// `stableID`, or a pattern's first example.
    public var id: String { action?.stableID ?? phrases[0] }

    init(_ summary: String, _ phrases: [String], hidden: [String] = [], _ action: VoiceAction?, note: String? = nil) {
        self.summary = summary
        self.phrases = phrases
        self.hidden = hidden
        self.action = action
        self.note = note
    }
}

public struct VoiceCommandGroup: Identifiable, Sendable {
    public let title: String
    public let commands: [VoiceCommand]
    public var id: String { title }
}

extension VoiceCommandParser {
    /// Every command, by category: the parser's phrase table is built from it, and the
    /// Voice Commands sheet lists it, so the two never drift.
    public static let catalog: [VoiceCommandGroup] = [
        VoiceCommandGroup(title: "Move", commands: [
            VoiceCommand("Next photo", ["next", "next photo", "forward", "skip"], hidden: ["next one", "necks", "nex"],
                         .key(.move(.right, extend: false))),
            VoiceCommand("Previous photo", ["previous", "back", "go back"], hidden: ["previous one", "prev"],
                         .key(.move(.left, extend: false))),
            VoiceCommand("First photo", ["first", "first photo"], hidden: ["first one"], .jump(.first)),
            VoiceCommand("Last photo", ["last", "last photo"], .jump(.last)),
            VoiceCommand("Move several photos", ["next 3", "next three photos", "forward 3", "skip 4", "back 2",
                                                 "previous two"], nil,
                         note: "A count after “next” moves only at the end of a phrase or before “photos”: “next three stars” is next, then three stars. “next to” and “skip for” do nothing."),
            VoiceCommand("Go to a photo", ["go to the 4th photo", "fourth photo", "photo 12", "go to number 12", "jump to 30",
                                           "go forward to the fourth", "skip to 10"], nil,
                         note: "Say “go to” or “photo”: “the second one” or “number one” alone does nothing. “forward to the fourth” goes to the 4th photo; “forward four” moves four."),
        ]),
        VoiceCommandGroup(title: "Find", commands: [
            VoiceCommand("Next unrated", ["next unrated"], hidden: ["next unrated one", "next not rated"], .jump(.nextUnrated)),
            VoiceCommand("Previous unrated", ["previous unrated"], hidden: ["prev unrated"], .jump(.seek(.unrated, forward: false))),
            VoiceCommand("Next pick", ["next pick", "next picked"], hidden: ["next pic"], .jump(.nextPick)),
            VoiceCommand("Previous pick", ["previous pick", "previous picked"], hidden: ["previous pic", "prev pick"],
                         .jump(.seek(.picked, forward: false))),
            VoiceCommand("Next reject", ["next reject", "next rejected"], .jump(.seek(.rejected, forward: true))),
            VoiceCommand("Previous reject", ["previous reject", "previous rejected"], hidden: ["prev reject"],
                         .jump(.seek(.rejected, forward: false))),
            VoiceCommand("Next unflagged", ["next unflagged"], .jump(.seek(.unflagged, forward: true))),
            VoiceCommand("Previous unflagged", ["previous unflagged"], hidden: ["prev unflagged"],
                         .jump(.seek(.unflagged, forward: false))),
            VoiceCommand("Next or previous with that many stars", ["next five star", "previous 3 star"], nil,
                         note: "Say “star”: “next five stars” is next, then five stars. One star is “next 1 star”."),
        ]),
        VoiceCommandGroup(title: "Rate and flag", commands: [
            VoiceCommand("5 stars", ["five", "five stars", "rate it five"], .key(.rate(5))),
            VoiceCommand("4 stars", ["four", "four stars"], .key(.rate(4))),
            VoiceCommand("3 stars", ["three", "three stars"], .key(.rate(3))),
            VoiceCommand("2 stars", ["two", "two stars"], .key(.rate(2))),
            VoiceCommand("1 star", ["one", "one star"], .key(.rate(1))),
            VoiceCommand("No stars", ["clear", "no stars", "clear rating"],
                         hidden: ["clear stars", "no star", "no rating", "unrate", "un rate", "zero stars", "zero star",
                                  "remove rating"], .key(.rate(0))),
            VoiceCommand("Pick", ["pick"], hidden: ["picked", "pic", "pik"], .key(.pick)),
            VoiceCommand("Reject", ["reject"], hidden: ["rejected"], .key(.reject)),
            VoiceCommand("Unflag", ["unflag", "clear flag"],
                         hidden: ["un flag", "remove flag", "no flag", "unpick", "un pick"], .key(.unflag)),
        ]),
        VoiceCommandGroup(title: "Several photos", commands: [
            VoiceCommand("Flag several", ["reject the next 3", "pick these three", "pick the next two", "unflag the next 4",
                                          "reject 3 photos", "pick next 2 photos"], nil,
                         note: "Two or more, said as “the next 3”, “next 3 photos”, “these 3” or “3 photos”. “The next 3” are the three after this photo; “these 3” and “3 photos” start with it. One undo step. “pick next one” is pick, then next."),
            VoiceCommand("Rate several", ["rate the next four three stars", "give the next 3 one star"], nil),
            VoiceCommand("Select several", ["select the next 3", "select these 4"], nil,
                         note: "Selects exactly those photos (“the next 3” are three, not counting this one) and moves to the first; keys then act on them."),
            VoiceCommand("Apply copied adjustments", ["apply to the next 3", "apply settings to these 4"], nil,
                         note: "Pastes what Copy’s options say, never onto the copied photo. One undo step."),
        ]),
        VoiceCommandGroup(title: "Color tags", commands: ColorTag.allCases.filter { $0 != .none }.map { t in
            let name = t.name.lowercased()
            return VoiceCommand("\(t.name) tag", [name, "\(name) tag"],
                                hidden: ["tag \(name)", "\(name) label", "label \(name)"] + (t == .red ? ["read", "read tag"] : []),
                                .key(.tag(t)))
        }),
        VoiceCommandGroup(title: "Filter", commands: [
            VoiceCommand("Show picks", ["show picks", "show picked"], hidden: ["show pics", "show only picks"], .showFlags(.picked)),
            VoiceCommand("Show rejects", ["show rejects", "show rejected"], hidden: ["show only rejects"], .showFlags(.rejected)),
            VoiceCommand("Show unflagged", ["show unflagged"], .showFlags(.unflagged)),
            VoiceCommand("Show all (any flag)", ["show all", "show all photos"], hidden: ["show everything"], .showFlags(.all)),
        ]),
        VoiceCommandGroup(title: "View", commands: [
            VoiceCommand("Grid", ["grid", "grid view"], hidden: ["show grid"], .key(.gridMode)),
            VoiceCommand("Viewer", ["viewer", "loupe"], hidden: ["view", "viewer mode", "single", "single view"], .key(.viewerMode)),
            VoiceCommand("Zoom in", ["zoom in"], hidden: ["closer"], .menu(.zoomIn)),
            VoiceCommand("Zoom out", ["zoom out"], .menu(.zoomOut)),
            VoiceCommand("Fit", ["fit", "zoom to fit"],
                         hidden: ["zoom fit", "fit to screen", "fit screen", "zoom to fit screen"], .menu(.zoomFit)),
            VoiceCommand("100%", ["hundred percent", "actual size"],
                         hidden: ["one hundred percent", "100 percent", "zoom 100", "zoom hundred", "zoom 100 percent",
                                  "zoom to 100 percent", "full size", "actual pixels"], .menu(.zoomActual)),
            VoiceCommand("Before / after", ["before after", "before and after"], hidden: ["before slash after", "show before"],
                         .key(.toggleBeforeAfter)),
            VoiceCommand("Compare", ["compare"], hidden: ["pin compare", "compare mode"], .key(.togglePinCompare)),
            VoiceCommand("Full screen", ["full screen"], hidden: ["fullscreen", "toggle full screen"], .key(.toggleFullScreen)),
            VoiceCommand("Exit full screen", ["exit full screen"],
                         hidden: ["leave full screen", "exit fullscreen", "leave fullscreen"], .setFullScreen(false)),
            VoiceCommand("Hide panels", ["hide panels"], hidden: ["hide panel", "hide tools"], .setPanelsHidden(true)),
            VoiceCommand("Show panels", ["show panels"], hidden: ["show panel", "show tools"], .setPanelsHidden(false)),
            VoiceCommand("Toggle panels", ["panels"], hidden: ["toggle panels"], .key(.togglePanels)),
            VoiceCommand("Rotate left", ["rotate left"], .menu(.rotateLeft)),
            VoiceCommand("Rotate right", ["rotate right"], .menu(.rotateRight)),
        ]),
        VoiceCommandGroup(title: "Edit", commands: [
            VoiceCommand("Undo", ["undo", "undo that"], hidden: ["un do", "undo last"], .key(.undo)),
            VoiceCommand("Redo", ["redo", "redo that"], hidden: ["re do"], .key(.redo)),
            VoiceCommand("Undo several", ["undo 3 times", "redo 2 steps"], nil,
                         note: "Say “times” or “steps”: “undo five” is undo, then five stars."),
            VoiceCommand("Copy adjustments", ["copy settings", "copy adjustments"], hidden: ["copy adjustment", "copy edits"],
                         .menu(.copyAdjustments)),
            VoiceCommand("Apply adjustments", ["apply settings", "apply adjustments", "paste settings"],
                         hidden: ["apply adjustment", "paste adjustments", "paste adjustment", "paste edits", "apply edits"],
                         .menu(.applyAdjustments)),
            VoiceCommand("Auto adjust", ["auto", "auto adjust"], hidden: ["auto tone", "autoadjust"], .menu(.autoAdjust)),
            VoiceCommand("AI changes", ["ai changes", "show ai changes"], hidden: ["what did ai change", "a i changes"],
                         .menu(.aiChanges)),
            VoiceCommand("Edit with words", ["edit warmer and a bit brighter"], nil,
                         note: "Opens Edit with Words with your phrase; press Return to apply it."),
        ]),
        VoiceCommandGroup(title: "Voice", commands: [
            VoiceCommand("What can I say?", ["what can I say", "help"], hidden: ["what do I say", "voice help",
                                                                               "show voice commands"], .help(true)),
            VoiceCommand("Close this list", ["close help"], hidden: ["hide help", "close voice commands"], .help(false)),
            VoiceCommand("Stop listening", ["stop listening"], hidden: ["stop voice", "voice off", "stop voice commands"],
                         .stopListening),
        ]),
    ]

    /// Commands a custom phrase can point at, by id (`VoiceCommand.id`).
    public static let commandsByID: [String: VoiceCommand] = Dictionary(
        catalog.flatMap(\.commands).filter { $0.action != nil }.map { ($0.id, $0) }) { a, _ in a }
}

/// What the Voice Commands sheet lists: `catalog`, then the user's own phrases.
public enum VoiceHelp {
    public struct Row: Equatable, Sendable, Identifiable {
        public var summary: String
        /// Quoted, as said ("“next 3”").
        public var phrases: [String]
        public var note: String?
        public var id: String { summary + phrases.joined() }
    }

    public struct Section: Equatable, Sendable, Identifiable {
        public var title: String
        public var rows: [Row]
        public var id: String { title }
    }

    public static func sections(custom: VoicePhraseBook = VoicePhraseBook()) -> [Section] {
        var out = VoiceCommandParser.catalog.map { g in
            Section(title: g.title, rows: g.commands.map { Row(summary: $0.summary, phrases: $0.phrases.map(quoted), note: $0.note) })
        }
        let mine = custom.rows
        if !mine.isEmpty {
            out.append(Section(title: "Your phrases", rows: mine.map { Row(summary: $0.command.summary, phrases: [quoted($0.phrase)]) }))
        }
        return out
    }

    private static func quoted(_ s: String) -> String { "“\(s)”" }

    public static let intro = """
        Say a phrase on its own; anything else said in the room is ignored. Phrases chain: “five next”, “pick next”.
        """
}
