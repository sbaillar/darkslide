import Foundation

/// Quit handling: what to ask, and what each answer means. Pure; the app
/// delegate only shows the alert and replies to AppKit.
public enum TerminationFlow {
    public enum Choice: String, Sendable, CaseIterable {
        case retry, quitAnyway, cancel, stopImportAndQuit, stopExportAndQuit

        public var title: String {
            switch self {
            case .retry: "Try Again"
            case .quitAnyway: "Quit Anyway"
            case .cancel: "Cancel"
            case .stopImportAndQuit: "Stop Import and Quit"
            case .stopExportAndQuit: "Stop Export and Quit"
            }
        }
    }

    public struct Alert: Equatable, Sendable {
        public var title: String
        public var message: String
        /// In button order; the first is the default (⏎).
        public var choices: [Choice]
        public var isCritical: Bool
    }

    /// What AppKit gets told.
    public enum Reply: Equatable, Sendable {
        case terminate
        case cancel
        /// Flush again, then decide again.
        case retry
    }

    /// `nil` = quit without asking.
    public static func alert(for decision: AppModel.TerminationDecision) -> Alert? {
        switch decision {
        case .ok:
            return nil
        case .failed(let message, let pending):
            return Alert(title: "\(pending) change\(pending == 1 ? "" : "s") couldn’t be saved",
                         message: "\(message)\n\nIf you quit now, \(pending == 1 ? "it is" : "they are") lost.",
                         choices: [.retry, .cancel, .quitAnyway], isCritical: true)
        case .unsavedInMemory(let count):
            return Alert(title: "\(count) rating\(count == 1 ? "" : "s") and edit\(count == 1 ? "" : "s") will be lost",
                         message: "This folder is open without a session store: ratings and edits are kept in "
                             + "memory only. Quitting discards them.",
                         choices: [.cancel, .quitAnyway], isCritical: true)
        case .importRunning:
            // The copy stops at its next chunk; its journal resumes it next time.
            return Alert(title: ImportWording.quitImportTitle, message: ImportWording.quitImportMessage,
                         choices: [.cancel, .stopImportAndQuit], isCritical: false)
        case .exportRunning:
            // The batch stops at its next photo; files already written stay.
            return Alert(title: "An export is running",
                         message: "Stop it and quit? Photos already exported stay; the rest aren't exported.",
                         choices: [.cancel, .stopExportAndQuit], isCritical: false)
        case .archiveFailed(let message):
            return Alert(title: "The last archive run failed",
                         message: "\(message)\n\nYour photos are safe on this Mac, but the archive doesn't have all of them. "
                             + "Darkslide tries again the next time it opens.",
                         choices: [.cancel, .quitAnyway], isCritical: false)
        }
    }

    public static func reply(for choice: Choice) -> Reply {
        switch choice {
        case .retry: .retry
        case .quitAnyway, .stopImportAndQuit, .stopExportAndQuit: .terminate
        case .cancel: .cancel
        }
    }

    /// Run the quit flow: decide, ask if needed, repeat on Retry. `ask` shows the alert.
    @MainActor
    public static func run(decide: () async -> AppModel.TerminationDecision,
                           ask: (Alert) -> Choice) async -> Bool {
        while true {
            let decision = await decide()
            guard let alert = alert(for: decision) else { return true }
            switch reply(for: ask(alert)) {
            case .terminate: return true
            case .cancel: return false
            case .retry: continue
            }
        }
    }
}
