import LuxtableAI
import Foundation
import Observation

/// Settings ▸ AI: which backends interpret typed edits. Jev (TypeSafe, over the
/// network, text only) is off until the user turns it on and saves an API key.
@MainActor @Observable
public final class AISettingsModel {
    @ObservationIgnored let jev: JevSettings
    @ObservationIgnored let foundationModelsAvailable: Bool

    /// Jev on/off (saved at once).
    public var jevEnabled: Bool { didSet { jev.enabled = jevEnabled } }
    /// The key being typed (never shown back once saved).
    public var keyEntry = ""
    public private(set) var hasKey: Bool
    public private(set) var message: String?

    public init(jev: JevSettings, foundationModelsAvailable: Bool = FoundationModelsEditService.isAvailable) {
        self.jev = jev
        self.foundationModelsAvailable = foundationModelsAvailable
        jevEnabled = jev.enabled
        hasKey = jev.hasKey
        if hasKey, jev.keyRejected { message = "Jev rejected the saved key. Enter a new one to use Jev." }
    }

    public func saveKey() {
        let key = keyEntry.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !key.isEmpty else { return }
        if jev.setKey(key) {
            hasKey = true
            keyEntry = ""
            message = "Key saved in your Keychain."
        } else {
            message = "The Keychain refused the key."
        }
    }

    public func removeKey() {
        _ = jev.setKey(nil)
        hasKey = jev.hasKey
        message = "Key removed."
    }

    /// The order typed edits go through, in words.
    public var order: String {
        var steps = ["Luxtable's own phrase rules"]
        if jevEnabled && hasKey { steps.append("TypeSafe Jev (text only, needs the network)") }
        if foundationModelsAvailable { steps.append("Apple Intelligence on this Mac") }
        return steps.joined(separator: " → ")
    }
}
