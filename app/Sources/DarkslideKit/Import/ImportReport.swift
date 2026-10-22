import Foundation

/// Outcome of one import as the *service* reports it. Only a report with
/// `safeToRemove` may ever produce "safe to remove"; only `allVerified` may
/// say the copies were verified. The card is the only other copy until then.
public struct ImportReport: Equatable, Sendable {
    public var filesTotal: Int
    /// Items copied (or already in place from an interrupted run), verified and registered.
    public var filesCopied: Int
    /// Files whose destination hash was re-read and matched the source (BLAKE3 in ds-import).
    public var filesVerified: Int
    public var bytesCopied: Int64
    /// ds-import's `is_complete` for this run's plan.
    public var isComplete: Bool
    /// Something stays only on the card (`cardOnly`, unreadable parts, files
    /// that aren't imported): "Eject Anyway…" needs a confirmation.
    public var ejectNeedsConfirmation: Bool
    public var scanErrors: [ImportIssue]
    public var skippedUnknown: [ImportIssue]
    public var failed: [ImportIssue]
    /// Copied and verified, but the session couldn't add them: not imported
    /// (opening the session or importing again adds them).
    public var registerErrors: [ImportIssue]
    /// Files the library already held (verified by full hash), not copied again.
    public var skippedDuplicates: Int
    public var cancelled: Bool
    /// The card went away mid-import.
    public var sourceLost: String?
    /// The session the files went into.
    public var sessionURL: URL?
    public var sessionName: String?
    /// Media files on the card with no verified copy in the library (from a
    /// fresh scan of the whole card after the run).
    public var cardOnly: [ImportIssue]
    /// Every media file on the card has a verified library copy and nothing
    /// else stays only on the card.
    public var safeToRemove: Bool
    /// Card files whose library copy was damaged and that were imported again.
    public var damagedLibraryCopies: [ImportIssue]
    /// The card the run read.
    public var volumeID: String?

    public init(filesTotal: Int, filesCopied: Int, filesVerified: Int, bytesCopied: Int64, isComplete: Bool? = nil,
                ejectNeedsConfirmation: Bool = false, scanErrors: [ImportIssue] = [], skippedUnknown: [ImportIssue] = [],
                failed: [ImportIssue] = [], registerErrors: [ImportIssue] = [], skippedDuplicates: Int = 0,
                cancelled: Bool = false, sourceLost: String? = nil, sessionURL: URL? = nil, sessionName: String? = nil,
                cardOnly: [ImportIssue] = [], safeToRemove: Bool? = nil, damagedLibraryCopies: [ImportIssue] = [],
                volumeID: String? = nil) {
        self.filesTotal = filesTotal
        self.filesCopied = filesCopied
        self.filesVerified = filesVerified
        self.bytesCopied = bytesCopied
        self.isComplete = isComplete ?? (filesTotal > 0 && filesVerified == filesTotal && filesCopied == filesTotal)
        self.ejectNeedsConfirmation = ejectNeedsConfirmation
        self.scanErrors = scanErrors
        self.skippedUnknown = skippedUnknown
        self.failed = failed
        self.registerErrors = registerErrors
        self.skippedDuplicates = skippedDuplicates
        self.cancelled = cancelled
        self.sourceLost = sourceLost
        self.sessionURL = sessionURL
        self.sessionName = sessionName
        self.cardOnly = cardOnly
        self.safeToRemove = safeToRemove ?? (self.isComplete && !ejectNeedsConfirmation && !cancelled
            && scanErrors.isEmpty && skippedUnknown.isEmpty && failed.isEmpty && registerErrors.isEmpty
            && cardOnly.isEmpty && sourceLost == nil)
        self.damagedLibraryCopies = damagedLibraryCopies
        self.volumeID = volumeID
    }

    public var allVerified: Bool {
        isComplete && !cancelled && filesTotal > 0 && filesVerified == filesTotal && filesCopied == filesTotal
    }

    /// Every file of *this plan* was copied, verified and registered (a stale or
    /// partial report for a different file count never counts).
    public func allVerified(for plan: ImportPlan) -> Bool {
        allVerified && filesTotal == plan.candidates.count && registerErrors.isEmpty
    }
}

/// The card's eject button (H1). Eject is never automatic.
///
/// - `safe`: every media file on the card has a verified library copy and
///   nothing else stays only on it: "Eject Card", no questions.
/// - otherwise "Eject Anyway…": the sheet lists what stays only on the card
///   and asks for a confirmation first.
public struct ImportEjectGate: Equatable, Sendable {
    public var verified: Bool
    /// Something stays only on the card (or nothing was verified).
    public var needsAcknowledgement: Bool
    /// The user confirmed "Eject Anyway".
    public var acknowledged: Bool
    public var canEject: Bool
    public var ejected: Bool

    public init(verified: Bool, needsAcknowledgement: Bool, acknowledged: Bool = false, canEject: Bool,
                ejected: Bool = false) {
        self.verified = verified
        self.needsAcknowledgement = needsAcknowledgement
        self.acknowledged = acknowledged
        self.canEject = canEject
        self.ejected = ejected
    }

    /// The card may be called safe to remove.
    public var safe: Bool { verified && !needsAcknowledgement }
    /// Show the eject button (always, for a local removable volume).
    public var offersEject: Bool { canEject && !ejected }
    /// Pressing it asks first.
    public var requiresConfirmation: Bool { !safe && !acknowledged }
    public var buttonTitle: String { safe ? "Eject Card" : "Eject Anyway…" }
}

/// Import services that verify copies (ds-import) conform and hand back the
/// report of the last `run`. Services that don't conform (the demo mock) are
/// treated as unverified, always.
public protocol ImportVerifying: ImportService {
    /// The report of `plan`'s run, waiting for it to finish (a stopped run too).
    func report(for plan: ImportPlan) async -> ImportReport?
}

/// Errors a verifying import service reports before copying anything.
public enum ImportRunError: Error, Equatable, LocalizedError {
    /// The card in the reader is not the one that was scanned (C1): scan again.
    case cardChanged
    /// The destination can't hold the import; nothing was created.
    case notEnoughSpace(String)

    public var errorDescription: String? {
        switch self {
        case .cardChanged:
            "A different card is in the reader. It was scanned again; check the selection and import."
        case .notEnoughSpace(let why): why
        }
    }
}

/// What the import UI may say. Pure, so "never claim verified or safe without
/// a report that says so" is enforced by tests rather than by view code.
public enum ImportWording {
    public static let liveUnavailableTitle = "Import isn’t available yet"
    public static let liveUnavailableMessage =
        "Verified import isn't available in this build. Darkslide never copies from cards without verifying, so it can't lose files. You can browse the card read-only."

    /// "1 photo", "3 photos", "2 videos", "3 photos and 1 video".
    public static func items(photos: Int, videos: Int) -> String {
        func n(_ k: Int, _ word: String) -> String { "\(k) \(word)\(k == 1 ? "" : "s")" }
        switch (photos, videos) {
        case (_, 0): return n(photos, "photo")
        case (0, _): return n(videos, "video")
        default: return "\(n(photos, "photo")) and \(n(videos, "video"))"
        }
    }

    /// "1 file", "3 files".
    public static func files(_ k: Int) -> String { "\(k) file\(k == 1 ? "" : "s")" }

    public static func doneTitle(_ s: ImportModel.ImportSummary) -> String {
        let what = items(photos: s.files - s.videos, videos: s.videos)
        if !s.verified, s.files > 0, !s.checkedByService { return "Copied \(what) (not verified)" }
        if s.files == 0 { return s.skippedDuplicates > 0 ? "Everything was already in the library" : "Nothing was imported" }
        return "Imported \(what)"
    }

    /// Per-file facts: what was verified, what went wrong, never "nothing was
    /// verified" when something was.
    public static func doneDetail(_ s: ImportModel.ImportSummary) -> String {
        let bytes = Formatters.bytes(s.bytes)
        let time = Formatters.duration(s.seconds)
        guard s.checkedByService else {
            return "\(bytes) copied into “\(s.sessionName)” in \(time). The copies were not verified, so keep the card."
        }
        var line = s.files > 0
            ? "\(files(s.files)) copied and verified into “\(s.sessionName)” (\(bytes) in \(time))."
            : "Nothing new was copied into “\(s.sessionName)”."
        var problems: [String] = []
        if !s.registerErrors.isEmpty { problems.append("\(files(s.registerErrors.count)) copied but not added to the session") }
        if !s.failed.isEmpty { problems.append("\(files(s.failed.count)) failed") }
        if !s.scanErrors.isEmpty {
            problems.append("\(s.scanErrors.count) part\(s.scanErrors.count == 1 ? "" : "s") of the card unreadable")
        }
        if !problems.isEmpty { line += " " + problems.joined(separator: "; ") + "." }
        return line
    }

    /// Line about the card; `nil` when there is nothing to say.
    public static func cardLine(_ s: ImportModel.ImportSummary) -> String? {
        if s.ejected {
            return s.safeToRemove
                ? "Card ejected. Every file on it has a verified copy in your library, so it is safe to remove."
                : "Card ejected. Keep it: the files listed above are only on the card."
        }
        if let err = s.ejectError { return "Couldn’t eject the card: \(err)" }
        if !s.checkedByService { return "Keep this card: these copies were not verified." }
        if s.safeToRemove { return "Every file on the card has a verified copy in your library. It is safe to remove." }
        if !s.stayOnCard.isEmpty { return "Keep this card: \(files(s.stayOnCard.count)) \(s.stayOnCard.count == 1 ? "is" : "are") only on it." }
        return "Keep this card: not every file on it has a verified copy in your library yet."
    }

    public static func readyLine(bytes: Int64, destination: String, verifies: Bool, free: Int64? = nil) -> String {
        let space = free.map { " · \(Formatters.bytes($0)) free" } ?? ""
        return verifies
            ? "\(Formatters.bytes(bytes)) to copy and verify into “\(destination)”\(space)"
            : "\(Formatters.bytes(bytes)) to copy into “\(destination)” (demo: not verified)"
    }

    /// Shown instead of the ready line when the selection can't fit.
    public static func notEnoughSpace(needed: Int64, free: Int64) -> String {
        "Not enough space: \(Formatters.bytes(needed)) to copy, \(Formatters.bytes(free)) free. Choose fewer files or another sessions folder."
    }

    /// VoiceOver label for a candidate tile.
    public static func tileLabel(_ c: ImportCandidate, selected: Bool, locked: Bool) -> String {
        let state = locked ? "already imported" : (selected ? "selected" : "not selected")
        return "\(c.filename), \(Formatters.bytes(c.size)), \(state)"
    }

    /// The Import button.
    public static func importButton(photos: Int, videos: Int) -> String {
        func n(_ k: Int, _ word: String) -> String { "\(k) \(word)\(k == 1 ? "" : "s")" }
        let parts = [(photos, "Photo"), (videos, "Video")].filter { $0.0 > 0 }.map { n($0.0, $0.1) }
        return parts.isEmpty ? "Import" : "Import " + parts.joined(separator: " and ")
    }

    /// Card panel note in the Import & Archive tab.
    public static func cardPanelNote(verifies: Bool, live: Bool) -> String {
        if verifies {
            return "Insert a memory card and Darkslide opens the import automatically. Files are copied, verified, and never deleted from the card."
        }
        return live
            ? "Import is coming soon. Insert a card to browse it read-only; Darkslide never copies from or deletes on a card until copies can be verified."
            : "Demo: imports are simulated and not verified. Nothing is ever deleted from a card."
    }

    /// Onboarding line about importing.
    public static func onboardingImport(verifies: Bool) -> (title: String, text: String) {
        verifies
            ? ("Insert a card, press Import", "Copies are verified before you're told they're safe. Nothing is ever deleted from a card.")
            : ("Insert a card to browse it", "Verified import is coming soon. Until then Darkslide never copies from or deletes on a card.")
    }

    /// Note under the backup destination.
    public static func backupNote(verifies: Bool) -> String {
        verifies ? "A second verified copy, written during import." : "A second copy, written during import (not verified in this build)."
    }

    public static func stoppedWhileFinishing(sessionName: String, ejected: Bool) -> String {
        "Import stopped while finishing. The copied files are in “\(sessionName)”."
            + (ejected ? " The card was ejected." : " The card was not ejected.")
    }

    /// Heading over the list of card files that were not imported.
    public static func issuesTitle(scanErrors: Int, skipped: Int) -> String {
        switch (scanErrors, skipped) {
        case (0, 0): ""
        case (0, let k): "\(k) file\(k == 1 ? "" : "s") on the card \(k == 1 ? "was" : "were") not imported"
        case (let e, 0): "\(e) part\(e == 1 ? "" : "s") of the card couldn’t be read"
        case (let e, let k): "\(e) unreadable, \(k) not imported"
        }
    }

    /// Heading over the files that stay only on the card after an import.
    public static func stayOnCardTitle(_ n: Int) -> String {
        "\(files(n)) \(n == 1 ? "is" : "are") only on the card"
    }

    /// Why the card stays: shown whenever something on it was not imported.
    public static func keepCardNote(scanErrors: Int) -> String {
        scanErrors > 0
            ? "Parts of the card couldn’t be read, so the import isn’t complete. Keep the card and check it on another reader."
            : "These files stay only on the card. Copy them yourself if you need them before formatting it."
    }

    /// The "Eject Anyway" confirmation.
    public static func ejectAnywayTitle(_ n: Int) -> String {
        n == 0 ? "Eject the card?" : "Eject with \(files(n)) only on the card?"
    }

    public static func ejectAnywayMessage(_ s: ImportModel.ImportSummary) -> String {
        guard s.checkedByService else { return "These copies were not verified. Keep the card until they are." }
        let names = s.stayOnCard.prefix(5).map { $0.url.lastPathComponent }.joined(separator: ", ")
        let more = s.stayOnCard.count > 5 ? " and \(s.stayOnCard.count - 5) more" : ""
        return names.isEmpty
            ? "Keep the card until the import is complete."
            : "\(names)\(more) \(s.stayOnCard.count == 1 ? "has" : "have") no copy in your library. Don’t format the card until they do."
    }

    public static let acknowledgeLabel = "I understand these files are only on the card"

    /// Line for an import that stopped part-way (journal kept for resume).
    public static func stopped(done: Int, total: Int) -> String {
        "Import stopped after \(done) of \(files(total)). The verified copies are kept; import again to continue."
    }

    /// The card went away mid-import (no raw path or errno).
    public static func cardRemoved(_ name: String) -> String {
        "\(name) was removed. Nothing was deleted from it; insert it again to continue the import."
    }

    /// Continuing an earlier, unfinished import of this card.
    public static func resumeNote(sessionName: String, left: Int) -> String {
        "Continuing the unfinished import into “\(sessionName)”: \(files(left)) left."
    }

    /// The launch prompt for an unfinished import.
    public static func resumePrompt(sessionName: String, done: Int, pending: Int) -> String {
        "The import into “\(sessionName)” didn’t finish: \(done) file\(done == 1 ? "" : "s") verified, \(pending) left. "
            + "Insert the card and import again to resume, or discard the unfinished copies."
    }

    /// Quitting while an import runs (M3).
    public static let quitImportTitle = "An import is running"
    public static let quitImportMessage = "Stop it and quit? It resumes the next time you import the card."

    public static func damagedNote(_ n: Int) -> String {
        "The library copy of \(files(n)) was damaged. \(n == 1 ? "It is" : "They are") imported again from the card."
    }

    public static func ejectHelp(canEject: Bool) -> String {
        canEject
            ? "Nothing is ever deleted from the card. Eject says whether every file on it has a verified copy in your library."
            : "Only local removable volumes can be ejected."
    }
}
