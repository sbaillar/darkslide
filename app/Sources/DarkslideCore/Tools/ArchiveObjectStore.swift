import DarkslideFFI
import Foundation

/// Where the archive goes (Settings ▸ Archive).
public enum ArchiveDestinationKind: String, CaseIterable, Sendable {
    /// rsync into a folder: a mounted volume (a NAS share, an external disk) or a local folder.
    case folder
    /// S3-compatible object storage (AWS S3, Backblaze B2, Wasabi, Cloudflare R2, MinIO).
    case object

    public var title: String {
        switch self {
        case .folder: "Folder"
        case .object: "Object storage"
        }
    }
}

/// The object storage key pair. Both keys live only in the Keychain (never in
/// preferences or files); the access key ID is shown, the secret never is. Read once
/// and cached, so the archive service can compare them every tick without a Keychain
/// call. `@unchecked`: the cache is lock-protected; the items are Sendable closures.
public final class ArchiveObjectKeys: @unchecked Sendable {
    public static let service = "casa.baillargeon.darkslide.archive"
    /// The app's live Keychain items.
    public static let shared = ArchiveObjectKeys()

    let accessKeyIDItem: KeychainItem
    let secretItem: KeychainItem
    private let lock = NSLock()
    private var cached: ObjectCredentials??

    public init(accessKeyID: KeychainItem = .keychain(service: service, account: "access-key-id"),
                secret: KeychainItem = .keychain(service: service, account: "secret-access-key")) {
        accessKeyIDItem = accessKeyID
        secretItem = secret
    }

    private static func trimmed(_ s: String?) -> String? {
        s.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.flatMap { $0.isEmpty ? nil : $0 }
    }

    /// Both keys, or nil when either is missing. Only a definite answer is cached: a
    /// Keychain that couldn't be read (locked, access denied) is asked again next time.
    public var credentials: ObjectCredentials? {
        lock.withLock {
            if let cached { return cached }
            let id = accessKeyIDItem.lookup(), secret = secretItem.lookup()
            let read: ObjectCredentials? = Self.trimmed(id.value).flatMap { id in
                Self.trimmed(secret.value).map { ObjectCredentials(accessKeyId: id, secretAccessKey: $0) }
            }
            if id != .failed && secret != .failed { cached = .some(read) }
            return read
        }
    }

    /// The access key ID, to show (nil: none saved).
    public var accessKeyID: String? { credentials?.accessKeyId ?? lock.withLock { Self.trimmed(accessKeyIDItem.read()) } }

    public var hasSecret: Bool { credentials != nil }

    /// Save the pair (a nil or empty `secret` keeps the saved one). Whether the Keychain took it.
    @discardableResult
    public func save(accessKeyID: String, secret: String?) -> Bool {
        let id = accessKeyID.trimmingCharacters(in: .whitespacesAndNewlines)
        let newSecret = Self.trimmed(secret)
        return lock.withLock {
            defer { cached = nil }
            guard accessKeyIDItem.write(id) else { return false }
            if let newSecret { return secretItem.write(newSecret) }
            return true
        }
    }

    /// Remove both keys from the Keychain. Whether both are gone.
    @discardableResult
    public func remove() -> Bool {
        lock.withLock {
            defer { cached = nil }
            let id = accessKeyIDItem.write(nil)
            let secret = secretItem.write(nil)
            return id && secret
        }
    }
}

extension ObjectStoreSettings {
    /// What is wrong with these settings, in words (nil: they can be used). The rules are
    /// ds-archive's (`ObjectStoreConfig::endpoint`, the prefix).
    public var problem: String? { archiveObjectSettingsProblem(settings: self) }

    /// `s3://bucket/prefix`, the way keys are laid out.
    public var layout: String {
        let p = prefix.split(separator: "/").filter { !$0.isEmpty }.map { "\($0)/" }.joined()
        return "s3://\(bucket.trimmingCharacters(in: .whitespaces))/\(p)"
    }
}

/// `ArchiveHttp` over `BlockingHTTP` (Rust calls it on a worker thread, one part of at
/// most 8 MB at a time): https, or plain http to a server on this network only (a LAN
/// MinIO; ds-archive's settings check says the same); redirects refused, so a signed
/// request never follows one to another host. Every response header is returned
/// (`ETag`, `x-amz-meta-*`, `Date`).
public final class URLSessionArchiveHTTP: ArchiveHttp, @unchecked Sendable {
    let http: BlockingHTTP

    public init(session: URLSession = BlockingHTTP.makeSession()) {
        http = BlockingHTTP(session: session, schemes: .httpsOrLocalHTTP, timeout: 120)
    }

    public func send(request: ArchiveHttpRequest) throws -> ArchiveHttpResponse {
        let a = try http.send(method: request.method, url: request.url, headers: request.headers.map { ($0.name, $0.value) },
                              body: request.body)
        return ArchiveHttpResponse(status: a.status, headers: a.headers.map { ArchiveHeader(name: $0.name, value: $0.value) },
                                   body: a.body)
    }
}
