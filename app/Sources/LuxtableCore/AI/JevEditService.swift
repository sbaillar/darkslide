import LuxtableAI
import Foundation
import Security

/// Natural-language edits through TypeSafe's Jev (a System One model: typed judgments,
/// not generated text). **Text only**: the request string is the only thing sent,
/// never pixels or file names.
///
/// One request asks, for each control, a *Score* question: how the photographer's
/// words want it changed, on seven ordered levels from "decrease a lot" to "increase
/// a lot". Code maps the chosen level to a step (the same steps as lt-ai's rules), and
/// `GuardedEditService` then keeps only the controls the words mention.
public struct JevEditService: NaturalLanguageEditService {
    public static let endpoint = URL(string: "https://api.typesafe.ai/v1/systemone")!

    public enum JevError: Error, Equatable {
        case disabled
        case http(Int)
        case malformed
    }

    /// Sends one request; injected in tests (no network).
    public typealias Transport = @Sendable (URLRequest) async throws -> (Data, Int)

    let transport: Transport
    /// `nil` = Jev is off or has no key: the next backend is used.
    let apiKey: @Sendable () -> String?

    public init(apiKey: @escaping @Sendable () -> String?, transport: @escaping Transport = JevEditService.urlSession) {
        self.apiKey = apiKey
        self.transport = transport
    }

    public static let urlSession: Transport = { request in
        let (data, response) = try await URLSession.shared.data(for: request)
        return (data, (response as? HTTPURLResponse)?.statusCode ?? 0)
    }

    /// The seven levels (index 3 = no change) and their step multipliers.
    static let multipliers: [Double] = [-2, -1, -0.5, 0, 0.5, 1, 2]

    /// Per question: the control, what "increase" means in words, the step per level unit.
    static let questions: [(id: String, what: String, more: String, less: String, step: Double)] = [
        ("warmth", "white balance", "warmer (more yellow/orange)", "cooler (more blue)", 500),
        ("tint", "tint", "more magenta", "more green", 10),
        ("exposure", "overall brightness", "brighter", "darker", 0.5),
        ("contrast", "contrast", "more contrast", "less contrast", 20),
        ("highlights", "highlights (the brightest areas)", "darker, recovered highlights", "brighter highlights", 25),
        ("shadows", "shadows (the darkest areas)", "brighter, lifted shadows", "darker shadows", 25),
        ("saturation", "colour saturation", "more saturated colour", "less saturated colour (towards black and white)", 20),
    ]

    /// The seven level descriptions for one question, "decrease a lot" first.
    static func criteria(_ q: (id: String, what: String, more: String, less: String, step: Double)) -> [String] {
        ["It asks for \(q.less), a lot", "It asks for \(q.less)", "It asks for \(q.less), a little",
         "It does not ask to change the \(q.what)",
         "It asks for \(q.more), a little", "It asks for \(q.more)", "It asks for \(q.more), a lot"]
    }

    static func body(for text: String) throws -> Data {
        var qs: [String: Any] = [:]
        for q in questions {
            qs[q.id] = [
                "type": "score",
                "instructions": "The photographer typed `request` to edit a photo. How does it ask for the photo's \(q.what) to change?",
                "criteria": criteria(q),
            ]
        }
        return try JSONSerialization.data(withJSONObject: ["model": "jev-latest", "state": ["request": text], "questions": qs])
    }

    /// Level chosen for one answer: the most probable level when probabilities come
    /// back, else the rounded expected position.
    static func level(_ answer: [String: Any], criteria: [String]) -> Int? {
        if let probs = answer["probabilities"] as? [String: Double], !probs.isEmpty,
           let best = probs.max(by: { $0.value < $1.value })?.key,
           let i = Int(best) ?? criteria.firstIndex(of: best) {
            return i
        }
        if let score = answer["score"] as? Double, let level = Safe.int(score) { return level }
        return nil
    }

    static func intent(from data: Data) throws -> EditIntent {
        guard let json = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let answers = json["answers"] as? [String: Any] else { throw JevError.malformed }
        var delta: [String: Double] = [:]
        for q in questions {
            guard let a = answers[q.id] as? [String: Any], let l = level(a, criteria: criteria(q)),
                  multipliers.indices.contains(l) else { continue }
            delta[q.id] = multipliers[l] * q.step
        }
        return EditIntent(temperatureShift: delta["warmth"] ?? 0, tintShift: delta["tint"] ?? 0,
                          exposureShift: delta["exposure"] ?? 0, contrastShift: delta["contrast"] ?? 0,
                          highlightsShift: delta["highlights"] ?? 0, shadowsShift: delta["shadows"] ?? 0,
                          saturationShift: delta["saturation"] ?? 0)
    }

    public func intent(for text: String) async throws -> EditIntent {
        guard let key = apiKey(), !key.isEmpty else { throw JevError.disabled }
        var request = URLRequest(url: Self.endpoint, timeoutInterval: 6)
        request.httpMethod = "POST"
        request.setValue("Bearer \(key)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try Self.body(for: text)
        let (data, status) = try await transport(request)
        guard status == 200 else { throw JevError.http(status) }
        return try Self.intent(from: data).sanitized()
    }
}

/// Jev settings: off by default; the API key lives in the Keychain, typed by the
/// user in Settings ▸ AI (the app has no other secret store to read it from).
/// `@unchecked`: UserDefaults is thread-safe; the key store is two Sendable closures.
public struct JevSettings: @unchecked Sendable {
    public static let enabledKey = "aiUseJev"
    /// Set when Jev answered 401/403 to the saved key; cleared by saving a new key.
    public static let keyRejectedKey = "aiJevKeyRejected"
    static let service = "app.luxtable.Luxtable.jev"

    /// Where the key is kept (the real Keychain in the app; a dictionary in tests).
    public struct KeyStore: Sendable {
        public var read: @Sendable () -> String?
        public var write: @Sendable (String?) -> Bool

        public init(read: @escaping @Sendable () -> String?, write: @escaping @Sendable (String?) -> Bool) {
            self.read = read
            self.write = write
        }

        /// The login Keychain, one generic password for the app; a key saved before the
        /// rename is copied the first time it is needed (`KeychainItem.migrating`).
        public static let keychain: KeyStore = {
            let item = KeychainItem(read: stored.read, write: stored.write)
                .migrating(from: KeychainItem.stored(service: KeychainItem.legacyService(service) ?? service, account: nil),
                           done: .preference("keychainMigrated.\(service)"))
            return KeyStore(read: item.read, write: item.write)
        }()

        /// The item itself, without the migration.
        static let stored = KeyStore(read: {
            let q: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service,
                                    kSecReturnData as String: true, kSecMatchLimit as String: kSecMatchLimitOne]
            var out: CFTypeRef?
            guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess, let d = out as? Data else { return nil }
            return String(data: d, encoding: .utf8)
        }, write: { key in
            let q: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service]
            guard let key, !key.isEmpty else {
                let s = SecItemDelete(q as CFDictionary)
                return s == errSecSuccess || s == errSecItemNotFound
            }
            // Update in place so a failed write keeps the previous key; add only when none exists.
            let value = Data(key.utf8)
            switch SecItemUpdate(q as CFDictionary, [kSecValueData as String: value] as CFDictionary) {
            case errSecSuccess:
                return true
            case errSecItemNotFound:
                var add = q
                add[kSecValueData as String] = value
                add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
                return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
            default:
                return false
            }
        })
    }

    public let defaults: UserDefaults
    public let store: KeyStore

    public init(defaults: UserDefaults, store: KeyStore = .keychain) {
        self.defaults = defaults
        self.store = store
    }

    public var enabled: Bool {
        get { defaults.bool(forKey: Self.enabledKey) }
        nonmutating set { defaults.set(newValue, forKey: Self.enabledKey) }
    }

    public var hasKey: Bool { !(store.read() ?? "").isEmpty }

    /// Jev refused the saved key (revoked or mistyped): typed edits quietly fell through to the next backend.
    public var keyRejected: Bool {
        get { defaults.bool(forKey: Self.keyRejectedKey) }
        nonmutating set { defaults.set(newValue, forKey: Self.keyRejectedKey) }
    }

    /// Whether `error` is Jev saying the key is not accepted.
    public static func isAuthFailure(_ error: Error) -> Bool {
        guard case JevEditService.JevError.http(let code) = error else { return false }
        return code == 401 || code == 403
    }

    @discardableResult
    public func setKey(_ key: String?) -> Bool {
        let saved = store.write(key?.trimmingCharacters(in: .whitespacesAndNewlines))
        if saved { keyRejected = false }
        return saved
    }

    /// The key when Jev is on and has one (read at each request, so Settings apply at once).
    public func activeKey() -> String? {
        guard enabled, let k = store.read(), !k.isEmpty else { return nil }
        return k
    }
}
