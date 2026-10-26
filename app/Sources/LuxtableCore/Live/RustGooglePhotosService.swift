import LuxtableFFI
import Foundation

/// `GooglePhotosService` over lt-gphotos: the protocol (refresh, retries, resumable
/// uploads, albums, batches) runs in Rust; requests go out through `URLSessionHTTP`.
/// The client ID, secret and refresh token are read from `GooglePhotosSettings` at
/// each call, so Settings apply at once.
public struct RustGooglePhotosService: GooglePhotosService {
    let settings: GooglePhotosSettings
    let http: any GphotosHttp

    public init(settings: GooglePhotosSettings, http: any GphotosHttp = URLSessionHTTP()) {
        self.settings = settings
        self.http = http
    }

    public var isSignedIn: Bool { settings.isSignedIn }

    func account() throws -> GphotosAccount {
        guard settings.isConfigured else { throw GooglePhotosError.notSetUp }
        guard let refresh = settings.refreshToken else { throw GooglePhotosError.notSignedIn }
        return GphotosAccount(clientId: settings.clientID, clientSecret: settings.clientSecret, refreshToken: refresh)
    }

    public func albums() async throws -> [GooglePhotosAlbum] {
        let account = try account()
        return try await Self.run { token in
            try await gphotosAlbums(http: http, account: account, token: token)
        }.map { GooglePhotosAlbum(id: $0.id, title: $0.title, itemCount: $0.itemCount.map(Int.init)) }
    }

    public func upload(_ files: [GooglePhotosFile], to target: GooglePhotosTarget,
                       progress: @escaping @Sendable (GooglePhotosProgress) -> Void) async throws -> GooglePhotosUpload {
        let account = try account()
        let album: GphotosAlbumChoice = target.albumID.map { .existing(id: $0, title: target.albumTitle) }
            ?? .named(title: target.albumTitle)
        let stream = ProgressStream()
        let listen = Task {
            for await e in stream.events where e.kind.hasPrefix("gphotos.") {
                progress(GooglePhotosProgress(done: Int(e.completed), total: Int(e.total), adding: e.kind == "gphotos.add"))
            }
        }
        let r: GphotosUploadReport
        do {
            r = try await Self.run { token in
                try await gphotosUpload(http: http, account: account,
                                        files: files.map { GphotosFile(path: $0.url.path, description: $0.description) },
                                        album: album, listener: stream, token: token)
            }
        } catch {
            stream.finish()
            listen.cancel()
            throw error
        }
        // Every step the upload reported reaches `progress` before it returns (the last,
        // "adding to the album", was dropped when the listener was cancelled here).
        stream.finish()
        await listen.value
        return GooglePhotosUpload(
            albumTitle: r.album?.title, uploaded: r.uploaded.map { URL(fileURLWithPath: $0.path) },
            failed: r.failed.map { GooglePhotosFailure(url: URL(fileURLWithPath: $0.path), reason: $0.reason) },
            cancelled: r.cancelled, error: r.error, signInNeeded: r.signInNeeded)
    }

    public func authorizationURL(redirectURI: String, challenge: String, state: String) throws -> URL {
        let s = try Self.mapped {
            try gphotosAuthorizationUrl(clientId: settings.clientID, redirectUri: redirectURI, codeChallenge: challenge,
                                        state: state)
        }
        guard let url = URL(string: s) else { throw GooglePhotosError.failed("bad authorization URL") }
        return url
    }

    public func code(fromRedirect target: String, state: String) throws -> String {
        try Self.mapped { try gphotosCodeFromRedirect(target: target, state: state) }
    }

    public func exchange(code: String, verifier: String, redirectURI: String) async throws -> String {
        let (id, secret) = (settings.clientID, settings.clientSecret)
        return try await Self.run { token in
            try await gphotosExchangeCode(http: http, clientId: id, clientSecret: secret, code: code,
                                          codeVerifier: verifier, redirectUri: redirectURI, token: token)
        }.refreshToken
    }

    public func revoke(refreshToken: String) async {
        _ = try? await Self.run { token in try await gphotosRevoke(http: http, refreshToken: refreshToken, token: token) }
    }

    /// Run an FFI call with a cancel token tied to the task; errors in words.
    static func run<T>(_ body: (CancelToken) async throws -> T) async throws -> T {
        let token = try CancelToken()
        return try await mapped {
            try await withTaskCancellationHandler { try await body(token) } onCancel: { try? token.cancel() }
        }
    }

    static func mapped<T>(_ body: () async throws -> T) async throws -> T {
        do { return try await body() } catch let e as DsError { throw error(e) }
    }

    static func mapped<T>(_ body: () throws -> T) throws -> T {
        do { return try body() } catch let e as DsError { throw error(e) }
    }

    static func error(_ e: DsError) -> Error {
        if case .Cancelled = e { return CancellationError() }
        return GooglePhotosError.failed(RenderError(e).message)
    }
}

/// `GphotosHttp` over `BlockingHTTP` (Rust calls it on a worker thread): HTTPS only, and
/// redirects refused, so the bearer token never follows a redirect to another host.
public final class URLSessionHTTP: GphotosHttp, @unchecked Sendable {
    let http: BlockingHTTP

    public init(session: URLSession = BlockingHTTP.makeSession()) {
        http = BlockingHTTP(session: session, schemes: .httpsOnly, timeout: 120)
    }

    public func send(request: GphotosHttpRequest) throws -> GphotosHttpResponse {
        let a = try http.send(method: request.method, url: request.url, headers: request.headers.map { ($0.name, $0.value) },
                              body: request.body)
        return GphotosHttpResponse(status: a.status, headers: a.headers.map { GphotosHeader(name: $0.name, value: $0.value) },
                                   body: a.body)
    }
}
