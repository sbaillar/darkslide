import Foundation
import Security

/// A recipe's Google Photos destination: its outputs are uploaded into this album
/// (one this app created; Google only lets an app add to its own albums).
public struct GooglePhotosTarget: Codable, Equatable, Hashable, Sendable {
    /// An album picked from the list; nil: the album called `albumTitle`, found by
    /// title among this app's albums or created.
    public var albumID: String?
    public var albumTitle: String

    public init(albumID: String? = nil, albumTitle: String = "Darkslide") {
        self.albumID = albumID
        self.albumTitle = albumTitle
    }

    public var summary: String { "Google Photos (\(albumTitle))" }
}

/// An album this app created in the user's Google Photos.
public struct GooglePhotosAlbum: Hashable, Sendable, Identifiable {
    public var id: String
    public var title: String
    public var itemCount: Int?

    public init(id: String, title: String, itemCount: Int? = nil) {
        self.id = id
        self.title = title
        self.itemCount = itemCount
    }
}

/// One exported file to upload, with the photo's caption (else title) as its description.
public struct GooglePhotosFile: Equatable, Sendable {
    public var url: URL
    public var description: String?

    public init(url: URL, description: String? = nil) {
        self.url = url
        self.description = description
    }
}

public struct GooglePhotosFailure: Equatable, Sendable, Hashable {
    public var url: URL
    public var reason: String

    public init(url: URL, reason: String) {
        self.url = url
        self.reason = reason
    }
}

/// While uploading: files finished so far, and whether they are being added to the album.
public struct GooglePhotosProgress: Equatable, Sendable {
    public var done: Int
    public var total: Int
    public var adding: Bool

    public init(done: Int, total: Int, adding: Bool = false) {
        self.done = done
        self.total = total
        self.adding = adding
    }
}

/// What one recipe's upload did.
public struct GooglePhotosUpload: Equatable, Sendable {
    public var recipeName: String
    public var albumTitle: String?
    public var uploaded: [URL]
    public var failed: [GooglePhotosFailure]
    public var cancelled: Bool
    /// Nothing was uploaded (the album couldn't be found or created): why.
    public var error: String?
    /// Google refused the account: sign in again in Settings.
    public var signInNeeded: Bool

    public init(recipeName: String = "", albumTitle: String? = nil, uploaded: [URL] = [], failed: [GooglePhotosFailure] = [],
                cancelled: Bool = false, error: String? = nil, signInNeeded: Bool = false) {
        self.recipeName = recipeName
        self.albumTitle = albumTitle
        self.uploaded = uploaded
        self.failed = failed
        self.cancelled = cancelled
        self.error = error
        self.signInNeeded = signInNeeded
    }

    /// "12 uploaded to Google Photos (Trip) · 2 failed".
    public var summary: String {
        if let error { return "Google Photos: \(error)" }
        var s = "\(uploaded.count) uploaded to Google Photos"
        if let albumTitle { s += " (\(albumTitle))" }
        if !failed.isEmpty { s += " · \(failed.count) failed" }
        if cancelled { s += " · stopped" }
        if signInNeeded { s += " · sign in again in Settings ▸ Google Photos" }
        return s
    }
}

/// Google Photos for the export sheet and Settings (`RustGooglePhotosService` in the app).
public protocol GooglePhotosService: Sendable {
    /// A client ID is set and a refresh token is in the Keychain.
    var isSignedIn: Bool { get }
    /// This app's albums (for the album picker).
    func albums() async throws -> [GooglePhotosAlbum]
    /// Upload `files` into `target`'s album; `progress` per photo. Failures per file are in the result.
    func upload(_ files: [GooglePhotosFile], to target: GooglePhotosTarget,
                progress: @escaping @Sendable (GooglePhotosProgress) -> Void) async throws -> GooglePhotosUpload

    // Sign-in steps (`GooglePhotosSignIn` runs them).
    /// The consent page for a loopback `redirectURI` and a PKCE S256 `challenge`.
    func authorizationURL(redirectURI: String, challenge: String, state: String) throws -> URL
    /// The code in the request the browser made to the loopback listener (checks `state`).
    func code(fromRedirect target: String, state: String) throws -> String
    /// Exchange the code for a refresh token.
    func exchange(code: String, verifier: String, redirectURI: String) async throws -> String
    /// Revoke the refresh token (best effort).
    func revoke(refreshToken: String) async
}

/// One generic-password Keychain item (a dictionary in tests).
public struct KeychainItem: Sendable {
    public var read: @Sendable () -> String?
    public var write: @Sendable (String?) -> Bool

    public init(read: @escaping @Sendable () -> String?, write: @escaping @Sendable (String?) -> Bool) {
        self.read = read
        self.write = write
    }

    /// The login Keychain item `service`/`account`; writing nil or "" deletes it. Written
    /// items are readable only while this Mac is unlocked and never migrate to another device
    /// (like `JevSettings.KeyStore`). Not the data-protection keychain: that needs a
    /// provisioning-profile entitlement the ad-hoc and self-signed builds don't have. Reads
    /// match regardless of the attribute, so an item written before it is still found and
    /// gets it the next time it is written.
    public static func keychain(service: String, account: String) -> KeychainItem {
        @Sendable func query() -> [String: Any] {
            [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
             kSecAttrAccount as String: account]
        }
        return KeychainItem(read: {
            var find = query()
            find[kSecReturnData as String] = true
            find[kSecMatchLimit as String] = kSecMatchLimitOne
            var out: CFTypeRef?
            guard SecItemCopyMatching(find as CFDictionary, &out) == errSecSuccess, let d = out as? Data else { return nil }
            return String(data: d, encoding: .utf8)
        }, write: { value in
            SecItemDelete(query() as CFDictionary)
            guard let value, !value.isEmpty else { return true }
            var add = query()
            add[kSecValueData as String] = Data(value.utf8)
            add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
            return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
        })
    }
}

/// The OAuth client built into the app at build time (`scripts/build-app.sh` writes
/// it into Info.plist from a git-ignored file or the environment; never committed).
public struct GooglePhotosClient: Equatable, Sendable {
    public static let idKey = "DSGoogleClientID"
    public static let secretKey = "DSGoogleClientSecret"

    public var id: String
    public var secret: String?

    public init(id: String, secret: String? = nil) {
        self.id = id
        self.secret = secret
    }

    /// The client in `info` (an Info.plist dictionary), if one was built in.
    public static func builtIn(_ info: [String: Any]?) -> GooglePhotosClient? {
        func value(_ key: String) -> String? {
            (info?[key] as? String).map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.flatMap { $0.isEmpty ? nil : $0 }
        }
        return value(idKey).map { GooglePhotosClient(id: $0, secret: value(secretKey)) }
    }
}

/// Settings ▸ Google Photos: which OAuth client signs in (the one built into the
/// app, or the user's own from Google Cloud Console, see docs/GOOGLE_PHOTOS.md) and
/// the signed-in account. The user's client ID is a preference; their client secret
/// and the refresh token live only in the Keychain, never in preferences or files.
/// `@unchecked`: UserDefaults is thread-safe; the items are Sendable closures.
public struct GooglePhotosSettings: @unchecked Sendable {
    public static let clientIDKey = "googlePhotosClientID"
    public static let service = "casa.baillargeon.darkslide.google-photos"

    public let defaults: UserDefaults
    public let clientSecretItem: KeychainItem
    public let refreshTokenItem: KeychainItem
    /// Built into this build of the app (nil: every user brings their own).
    public let builtIn: GooglePhotosClient?

    public init(defaults: UserDefaults,
                clientSecret: KeychainItem = .keychain(service: service, account: "client-secret"),
                refreshToken: KeychainItem = .keychain(service: service, account: "refresh-token"),
                builtIn: GooglePhotosClient? = GooglePhotosClient.builtIn(Bundle.main.infoDictionary)) {
        self.defaults = defaults
        clientSecretItem = clientSecret
        refreshTokenItem = refreshToken
        self.builtIn = builtIn
    }

    /// The user's own client ID (advanced; "" = none), which wins over the built-in one.
    public var clientIDOverride: String {
        get { (defaults.string(forKey: Self.clientIDKey) ?? "").trimmingCharacters(in: .whitespacesAndNewlines) }
        nonmutating set {
            let v = newValue.trimmingCharacters(in: .whitespacesAndNewlines)
            if v.isEmpty { defaults.removeObject(forKey: Self.clientIDKey) } else { defaults.set(v, forKey: Self.clientIDKey) }
        }
    }

    /// The client that signs in: the user's own, else the built-in one ("" when neither).
    public var clientID: String {
        let own = clientIDOverride
        return own.isEmpty ? builtIn?.id ?? "" : own
    }

    /// The secret that goes with `clientID`.
    public var clientSecret: String? {
        clientIDOverride.isEmpty ? builtIn?.secret : nonEmpty(clientSecretItem.read())
    }

    public var refreshToken: String? { nonEmpty(refreshTokenItem.read()) }

    public var usesBuiltInClient: Bool { clientIDOverride.isEmpty && builtIn != nil }
    public var isConfigured: Bool { !clientID.isEmpty }
    public var isSignedIn: Bool { isConfigured && refreshToken != nil }

    private func nonEmpty(_ s: String?) -> String? {
        s.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.flatMap { $0.isEmpty ? nil : $0 }
    }
}

public enum GooglePhotosError: Error, Equatable, LocalizedError {
    case notSetUp
    case notSignedIn
    case keychain
    case failed(String)

    public var errorDescription: String? {
        switch self {
        case .notSetUp: "This build has no Google sign-in built in: add your own OAuth client ID first (see “How to set up”)."
        case .notSignedIn: "Sign in to Google Photos in Settings ▸ Google Photos."
        case .keychain: "The Keychain refused to save the Google sign-in."
        case .failed(let m): m
        }
    }
}
