import Foundation

/// Spoken confirmations for VoiceOver: what a sighted user sees flash by (a toast) or sit in
/// the toolbar (a failed save) is also announced.
@MainActor
public enum Announcer {
    public enum Priority: Equatable, Sendable { case normal, urgent }

    /// Where announcements go (tests replace it).
    static var sink: (String, Priority) -> Void = { text, priority in
        PlatformAccessibility.announce(text, urgent: priority == .urgent)
    }

    public static func announce(_ text: String, _ priority: Priority = .normal) {
        guard !text.isEmpty else { return }
        sink(text, priority)
    }

    public static func announce(_ toast: Toast) {
        announce(toast.message, toast.isError ? .urgent : .normal)
    }

    /// What to say when the save state becomes `state`: only data-safety problems speak.
    public static func announcement(for state: SaveCoordinator.State) -> (text: String, priority: Priority)? {
        switch state {
        case .failed(let message, let pending):
            ("\(pending) unsaved change\(pending == 1 ? "" : "s"). Saving failed: \(message)", .urgent)
        case .dropped(let message, let count):
            ("\(count) change\(count == 1 ? "" : "s") not saved. \(message)", .urgent)
        case .saved, .pending, .saving:
            nil
        }
    }

    /// Announces a save problem once, when the state *enters* failed or dropped; a failed
    /// save's pending count growing with every further edit is not said again.
    public static func announce(saveState state: SaveCoordinator.State, from old: SaveCoordinator.State) {
        guard kind(state) != kind(old), let a = announcement(for: state) else { return }
        announce(a.text, a.priority)
    }

    private static func kind(_ s: SaveCoordinator.State) -> Int {
        switch s {
        case .saved: 0
        case .pending: 1
        case .saving: 2
        case .failed: 3
        case .dropped: 4
        }
    }
}
