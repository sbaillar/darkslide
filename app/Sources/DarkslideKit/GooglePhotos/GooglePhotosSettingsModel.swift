import Foundation
import Observation

/// Settings ▸ Google Photos: sign in and out. A build with a built-in OAuth client
/// needs nothing else; without one (or for advanced use) the user adds their own.
@MainActor @Observable
public final class GooglePhotosSettingsModel {
    @ObservationIgnored let settings: GooglePhotosSettings
    @ObservationIgnored let service: any GooglePhotosService
    /// Runs the sign-in (the consent page in the app; a stub in tests).
    @ObservationIgnored let signIn: @MainActor () async throws -> Void

    /// The user's own client ID being typed (saved with `saveClient()`).
    public var clientIDEntry: String
    /// The client secret being typed (never shown back once saved).
    public var secretEntry = ""
    /// The user's own client ID ("" = none: the built-in client, if any, signs in).
    public private(set) var ownClientID: String
    /// The client that signs in ("" = none).
    public private(set) var clientID: String
    /// The user's own client secret is in the Keychain.
    public private(set) var hasSecret: Bool
    public private(set) var isSignedIn: Bool
    public private(set) var busy = false
    public private(set) var message: String?

    public init(settings: GooglePhotosSettings, service: any GooglePhotosService,
                signIn: @escaping @MainActor () async throws -> Void) {
        self.settings = settings
        self.service = service
        self.signIn = signIn
        ownClientID = settings.clientIDOverride
        clientID = settings.clientID
        clientIDEntry = settings.clientIDOverride
        hasSecret = !(settings.clientSecretItem.read() ?? "").isEmpty
        isSignedIn = settings.isSignedIn
    }

    /// This build signs in with its own client: no setup needed.
    public var hasBuiltInClient: Bool { settings.builtIn != nil }

    /// How to make the OAuth client (the full guide is docs/GOOGLE_PHOTOS.md).
    public static let setupSteps = [
        "In Google Cloud Console, create a project and enable the Photos Library API.",
        "Under Google Auth Platform, set up the consent screen (External) and add yourself as a test user.",
        "Add the scopes photoslibrary.appendonly and photoslibrary.readonly.appcreateddata.",
        "Create an OAuth client of type “Desktop app”, then paste its client ID and client secret here.",
    ]

    public var canSaveClient: Bool {
        let id = clientIDEntry.trimmingCharacters(in: .whitespacesAndNewlines)
        return !isSignedIn && (id != ownClientID || !secretEntry.isEmpty) && !id.isEmpty
    }

    /// Save the client ID (a preference) and the secret, if typed (the Keychain).
    public func saveClient() {
        guard canSaveClient else { return }
        settings.clientIDOverride = clientIDEntry
        ownClientID = settings.clientIDOverride
        clientID = settings.clientID
        clientIDEntry = ownClientID
        if !secretEntry.isEmpty {
            guard settings.clientSecretItem.write(secretEntry.trimmingCharacters(in: .whitespacesAndNewlines)) else {
                message = "The Keychain refused the client secret."
                return
            }
            secretEntry = ""
            hasSecret = true
        }
        message = ownClientID.hasSuffix(".apps.googleusercontent.com")
            ? "Saved." : "Saved. Client IDs usually end in .apps.googleusercontent.com."
    }

    /// Forget the user's own client (only while signed out); the built-in one, if
    /// any, signs in again.
    public func removeClient() {
        guard !isSignedIn else { return }
        settings.clientIDOverride = ""
        _ = settings.clientSecretItem.write(nil)
        ownClientID = ""
        clientID = settings.clientID
        clientIDEntry = ""
        hasSecret = false
        message = hasBuiltInClient ? "Your client was removed; Darkslide's own is used." : "Client removed."
    }

    public var canSignIn: Bool { !clientID.isEmpty && !isSignedIn && !busy }

    public func signInNow() async {
        guard canSignIn else { return }
        busy = true
        message = "Waiting for Google… finish signing in in the window that opened."
        defer { busy = false }
        do {
            try await signIn()
            isSignedIn = settings.isSignedIn
            message = "Signed in. Exports can now go to Google Photos."
        } catch is CancellationError {
            message = "Sign-in cancelled."
        } catch {
            message = (error as? LocalizedError)?.errorDescription ?? "\(error)"
        }
    }

    /// Revoke the token at Google (best effort) and forget it.
    public func signOut() async {
        guard isSignedIn, !busy else { return }
        busy = true
        defer { busy = false }
        if let token = settings.refreshToken { await service.revoke(refreshToken: token) }
        _ = settings.refreshTokenItem.write(nil)
        isSignedIn = settings.isSignedIn
        message = "Signed out."
    }

    /// The account state in words.
    public var status: String {
        if clientID.isEmpty { return "Not set up" }
        return isSignedIn ? "Signed in" : "Signed out"
    }
}
