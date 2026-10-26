import Foundation
import Security

/// A recipe's Google Photos destination: its outputs are uploaded into this album
/// (one this app created; Google only lets an app add to its own albums).
public struct GooglePhotosTarget: Codable, Equatable, Hashable, Sendable {
    /// An album picked from the list; nil: the album called `albumTitle`, found by
    /// title among this app's albums or created.
    public var albumID: String?
    public var albumTitle: String

    public init(albumID: String? = nil, albumTitle: String = "Luxtable") {
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

/// What reading a Keychain item found.
public enum KeychainLookup: Equatable, Sendable {
    case found(String)
    /// There is no such item.
    case notFound
    /// The Keychain couldn't be read (locked, access denied, damaged): ask again later.
    case failed

    public var value: String? {
        if case .found(let s) = self { return s }
        return nil
    }
}

/// One generic-password Keychain item (a dictionary in tests).
public struct KeychainItem: Sendable {
    public var read: @Sendable () -> String?
    /// Whether the value was written, or (nil or "") the item is gone.
    public var write: @Sendable (String?) -> Bool
    /// `read`, telling a missing item from a Keychain that couldn't be read.
    public var lookup: @Sendable () -> KeychainLookup

    /// Without `lookup`, a nil `read` counts as "no such item".
    public init(read: @escaping @Sendable () -> String?, write: @escaping @Sendable (String?) -> Bool,
                lookup: (@Sendable () -> KeychainLookup)? = nil) {
        self.read = read
        self.write = write
        self.lookup = lookup ?? { read().map(KeychainLookup.found) ?? .notFound }
    }

    /// The login Keychain item `service`/`account`; writing nil or "" deletes it. Written
    /// items are readable only while this Mac is unlocked and never migrate to another device
    /// (like `JevSettings.KeyStore`). Not the data-protection keychain: that needs a
    /// provisioning-profile entitlement the ad-hoc and self-signed builds don't have. Reads
    /// match regardless of the attribute, so an item written before it is still found and
    /// gets it the next time it is written. In a test run the item is kept in memory
    /// instead (`inTestProcess`), so no test reads or replaces the user's own keys.
    ///
    /// A key saved before the rename (under the old app's service, see
    /// `legacyService`) is copied over the first time this item is read and missing
    /// ([`migrating(from:done:)`]): never before it is needed, so an integration that is
    /// off never reads (or asks for) its old key. The old item is left as it is.
    public static func keychain(service: String, account: String) -> KeychainItem {
        let item = stored(service: service, account: account)
        guard let old = legacyService(service) else { return item }
        return item.migrating(from: stored(service: old, account: account),
                              done: .preference("keychainMigrated.\(service).\(account)"))
    }

    /// Migration source: the services the app's Keychain items had before the rename
    /// (its old bundle identifier, `LegacyMigration.legacyBundleID`).
    static func legacyService(_ service: String) -> String? {
        let prefix = "app.luxtable.Luxtable"
        guard service.hasPrefix(prefix) else { return nil }
        return LegacyMigration.legacyBundleID + service.dropFirst(prefix.count)
    }

    /// Whether a lazy copy from the old item was settled (so a key deleted later isn't
    /// brought back from the old item).
    public struct MigrationFlag: Sendable {
        public var get: @Sendable () -> Bool
        public var set: @Sendable () -> Void

        public init(get: @escaping @Sendable () -> Bool, set: @escaping @Sendable () -> Void) {
            self.get = get
            self.set = set
        }

        /// In the app's preferences (`AppModel.appDefaults`'s domain); in memory in a test run.
        static func preference(_ key: String) -> MigrationFlag {
            if inTestProcess {
                let flag = TestKeychain.Flag()
                return MigrationFlag(get: { flag.value }, set: { flag.value = true })
            }
            let suite = AppModel.defaultsSuiteName(arguments: CommandLine.arguments,
                                                   environment: ProcessInfo.processInfo.environment)
            let defaults: @Sendable () -> UserDefaults = { suite.flatMap(UserDefaults.init(suiteName:)) ?? .standard }
            return MigrationFlag(get: { defaults().bool(forKey: key) }, set: { defaults().set(true, forKey: key) })
        }
    }

    /// This item, with `legacy` copied into it the first time it is looked up and found
    /// missing. Settled (`done`) once the old item was found and copied, or definitely
    /// isn't there, or this item was written: from then on only this item counts. A
    /// Keychain that couldn't be read (locked, access denied) is asked again next time.
    public func migrating(from legacy: KeychainItem, done: MigrationFlag) -> KeychainItem {
        let current = self
        let lookup: @Sendable () -> KeychainLookup = {
            let mine = current.lookup()
            guard mine == .notFound, !done.get() else { return mine }
            switch legacy.lookup() {
            case .found(let value):
                guard current.write(value) else { return .found(value) }
                done.set()
                return .found(value)
            case .notFound:
                done.set()
                return .notFound
            case .failed:
                return mine
            }
        }
        return KeychainItem(read: { lookup().value }, write: { value in
            done.set()
            return current.write(value)
        }, lookup: lookup)
    }

    /// The item itself, without the migration (`account` nil: any account of the service).
    static func stored(service: String, account: String?) -> KeychainItem {
        if inTestProcess { return TestKeychain.item(service: service, account: account ?? "") }
        @Sendable func query() -> [String: Any] {
            var q: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service]
            if let account { q[kSecAttrAccount as String] = account }
            return q
        }
        let lookup: @Sendable () -> KeychainLookup = {
            var find = query()
            find[kSecReturnData as String] = true
            find[kSecMatchLimit as String] = kSecMatchLimitOne
            var out: CFTypeRef?
            let status = SecItemCopyMatching(find as CFDictionary, &out)
            return Self.lookup(status: status, data: out as? Data)
        }
        return KeychainItem(read: { lookup().value }, write: { value in
            let deleted = SecItemDelete(query() as CFDictionary)
            guard let value, !value.isEmpty else { return Self.removed(deleted) }
            var add = query()
            add[kSecValueData as String] = Data(value.utf8)
            add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
            return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
        }, lookup: lookup)
    }

    /// What `SecItemCopyMatching`'s `status` (and `data`) say about the item.
    static func lookup(status: OSStatus, data: Data?) -> KeychainLookup {
        switch status {
        case errSecSuccess: data.flatMap { String(data: $0, encoding: .utf8) }.map(KeychainLookup.found) ?? .failed
        case errSecItemNotFound: .notFound
        default: .failed
        }
    }

    /// Whether `SecItemDelete`'s `status` leaves no item (one that wasn't there counts).
    static func removed(_ status: OSStatus) -> Bool {
        status == errSecSuccess || status == errSecItemNotFound
    }

    /// A test run: `swift test` hands the `.xctest` bundle to its runner by path. Debug
    /// builds only; a release build always uses the Keychain.
    static let inTestProcess: Bool = {
        #if DEBUG
        let p = ProcessInfo.processInfo
        return p.environment["XCTestConfigurationFilePath"] != nil || p.environment["XCTestBundlePath"] != nil
            || p.arguments.contains { $0.hasSuffix(".xctest") || $0.contains(".xctest/") }
        #else
        return false
        #endif
    }()
}

/// The Keychain of a test run: items by service and account, in memory, shared by
/// every `KeychainItem.keychain` item of the process as the real Keychain would be.
enum TestKeychain {
    /// A settled-migration flag of a test run.
    final class Flag: @unchecked Sendable {
        private let lock = NSLock()
        private var on = false
        var value: Bool {
            get { lock.withLock { on } }
            set { lock.withLock { on = newValue } }
        }
    }

    private static let lock = NSLock()
    nonisolated(unsafe) private static var items: [String: String] = [:]

    static func item(service: String, account: String) -> KeychainItem {
        let key = "\(service)\u{0}\(account)"
        return KeychainItem(read: { lock.withLock { items[key] } }, write: { value in
            lock.withLock { items[key] = value.flatMap { $0.isEmpty ? nil : $0 } }
            return true
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
    public static let service = "app.luxtable.Luxtable.google-photos"

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
