import Foundation

/// The user's own phrases (Settings ▸ Voice): "keeper" → Pick, "bin it" → Reject.
/// Stored as the phrase as typed → command id (`VoiceCommand.id`, stable across catalog
/// edits); matched by its normalized words.
public struct VoicePhraseBook: Equatable, Sendable {
    public private(set) var phrases: [String: String] = [:]

    /// From what was stored. Each phrase is checked again as if added now (alphabetically):
    /// one whose command is gone, or that a newer built-in command now uses, is dropped.
    public init(_ stored: [String: String] = [:]) {
        for (phrase, id) in stored.sorted(by: { $0.key < $1.key }) { _ = add(phrase, for: id) }
    }

    /// The phrases voice hears: the user's own, then the plug-ins' (a plug-in phrase that
    /// clashes with one already in the book, or can't be heard, is left out).
    public static func effective(user: [String: String], plugin: [String: String] = PluginContent.current.phrases)
        -> VoicePhraseBook {
        var book = VoicePhraseBook(user)
        for (phrase, id) in plugin.sorted(by: { $0.key < $1.key }) { _ = book.add(phrase, for: id) }
        return book
    }

    /// What the parser matches.
    public var actions: [String: VoiceAction] {
        Dictionary(phrases.compactMap { p, id in
            VoiceCommandParser.commandsByID[id]?.action.map { (VoiceCommandParser.normalize(p), $0) }
        }) { a, _ in a }
    }

    /// The phrases, alphabetically, with their commands.
    public var rows: [(phrase: String, command: VoiceCommand)] {
        phrases.keys.sorted().compactMap { p in VoiceCommandParser.commandsByID[phrases[p]!].map { (p, $0) } }
    }

    /// Why `phrase` can't be added for `commandID` (`nil`: it can).
    public func refusal(_ phrase: String, for commandID: String) -> String? {
        guard VoiceCommandParser.commandsByID[commandID] != nil else { return "Choose a command for it." }
        let key = VoiceCommandParser.normalize(phrase)
        let shown = display(phrase)
        if key.isEmpty {
            return "“\(shown)” has no words to listen for (“the”, “please” and the like are ignored)."
        }
        if key.split(separator: " ").first == "edit" {
            return "Phrases starting with “edit” are Edit with Words instructions."
        }
        if let (other, id) = phrases.first(where: { VoiceCommandParser.normalize($0.key) == key }) {
            let name = VoiceCommandParser.commandsByID[id]?.summary ?? id
            return id == commandID ? "“\(other)” is already your phrase for \(name)."
                : "“\(other)” is already your phrase for \(name); remove it first."
        }
        if key.contains("?") || key.contains("#") || key.contains("→") {
            return "“\(shown)” has a number after “photo”, “number” or “go to”: voice hears that as a position."
        }
        if let meaning = VoiceCommandParser.parse(phrase, custom: actions) {
            return "“\(shown)” already means \(meaning.map(\.label).joined(separator: " · "))."
        }
        // It must be heard: a phrase starting with a word voice skips ("photo finish",
        // "the next keeper") would never fire.
        if let action = VoiceCommandParser.commandsByID[commandID]?.action,
           VoiceCommandParser.parse(phrase, custom: actions.merging([key: action]) { a, _ in a }) != [action] {
            return "“\(shown)” can’t be heard as one command: try other words (“photo”, “number” and “the next” at the start are taken)."
        }
        return nil
    }

    /// Add `phrase` for `commandID`; returns why it was refused (`nil`: added).
    public mutating func add(_ phrase: String, for commandID: String) -> String? {
        if let why = refusal(phrase, for: commandID) { return why }
        phrases[display(phrase)] = commandID
        return nil
    }

    /// Remove the phrase that sounds like `phrase` ("Keeper!" removes "keeper").
    public mutating func remove(_ phrase: String) {
        let key = VoiceCommandParser.normalize(phrase)
        phrases = phrases.filter { VoiceCommandParser.normalize($0.key) != key }
    }

    private func display(_ phrase: String) -> String {
        phrase.trimmingCharacters(in: .whitespacesAndNewlines.union(.punctuationCharacters))
    }

    /// Commands a phrase can point at, by category (Settings ▸ Voice's picker).
    public static var targets: [VoiceCommandGroup] {
        VoiceCommandParser.catalog.compactMap { g in
            let commands = g.commands.filter { $0.action != nil }
            return commands.isEmpty ? nil : VoiceCommandGroup(title: g.title, commands: commands)
        }
    }
}

extension AppSettings {
    /// Settings ▸ Voice: your own phrase for a command, stored in the prefs; `voice`
    /// hears it at once. Returns why it was refused (`nil`: added).
    public func addVoicePhrase(_ phrase: String, for commandID: String, voice: VoiceController) -> String? {
        var book = VoicePhraseBook(voicePhrases)
        if let why = book.add(phrase, for: commandID) { return why }
        voicePhrases = book.phrases
        voice.customPhrases = VoicePhraseBook.effective(user: book.phrases).actions
        return nil
    }

    public func removeVoicePhrase(_ phrase: String, voice: VoiceController) {
        var book = VoicePhraseBook(voicePhrases)
        book.remove(phrase)
        voicePhrases = book.phrases
        voice.customPhrases = VoicePhraseBook.effective(user: book.phrases).actions
    }
}
