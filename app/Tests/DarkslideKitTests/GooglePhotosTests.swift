import DarkslideFFI
import Foundation
import Network
import Testing
@testable import DarkslideKit

// MARK: - Doubles

/// A Keychain item kept in memory.
private final class MemoryItem: @unchecked Sendable {
    private let lock = NSLock()
    private var value: String?
    var refuse = false
    init(_ v: String? = nil) { value = v }
    var item: KeychainItem {
        KeychainItem(read: { self.lock.withLock { self.value } },
                     write: { v in self.lock.withLock { if self.refuse { return false }; self.value = v; return true } })
    }
    var current: String? { lock.withLock { value } }
}

@MainActor
private func settings(clientID: String = "id.apps.googleusercontent.com", secret: String? = "sec",
                      refresh: String? = "refresh-1", builtIn: GooglePhotosClient? = nil)
    -> (GooglePhotosSettings, MemoryItem, MemoryItem) {
    let secretItem = MemoryItem(secret), refreshItem = MemoryItem(refresh)
    let s = GooglePhotosSettings(defaults: testDefaults(), clientSecret: secretItem.item, refreshToken: refreshItem.item,
                                 builtIn: builtIn)
    s.clientIDOverride = clientID
    return (s, secretItem, refreshItem)
}

/// A scripted `GooglePhotosService`.
private final class FakeGooglePhotos: GooglePhotosService, @unchecked Sendable {
    private let lock = NSLock()
    var signedIn = true
    var albumList: [GooglePhotosAlbum] = [GooglePhotosAlbum(id: "b", title: "Beta"), GooglePhotosAlbum(id: "a", title: "alpha")]
    var albumsError: Error?
    var uploadError: Error?
    /// Fail these file names.
    var failNames: Set<String> = []
    var gate: AsyncStream<Void>?
    private(set) var uploads: [([GooglePhotosFile], GooglePhotosTarget)] = []
    private(set) var revoked: [String] = []
    var exchangeError: Error?
    var codeError: Error?

    var isSignedIn: Bool { signedIn }
    func albums() async throws -> [GooglePhotosAlbum] {
        if let albumsError { throw albumsError }
        return albumList
    }
    func upload(_ files: [GooglePhotosFile], to target: GooglePhotosTarget,
                progress: @escaping @Sendable (GooglePhotosProgress) -> Void) async throws -> GooglePhotosUpload {
        lock.withLock { uploads.append((files, target)) }
        if let gate { for await _ in gate { break } }
        if let uploadError { throw uploadError }
        for i in files.indices { progress(GooglePhotosProgress(done: i, total: files.count)) }
        progress(GooglePhotosProgress(done: files.count, total: files.count, adding: true))
        let failed = files.filter { failNames.contains($0.url.lastPathComponent) }
        return GooglePhotosUpload(albumTitle: target.albumID == nil ? nil : "Picked",
                                  uploaded: files.filter { !failNames.contains($0.url.lastPathComponent) }.map(\.url),
                                  failed: failed.map { GooglePhotosFailure(url: $0.url, reason: "Failed") })
    }
    func authorizationURL(redirectURI: String, challenge: String, state: String) throws -> URL {
        URL(string: "https://consent.example/?redirect=\(redirectURI)&state=\(state)")!
    }
    func code(fromRedirect target: String, state: String) throws -> String {
        guard target.contains("state=\(state)") else { throw GooglePhotosError.failed("state mismatch") }
        if let codeError { throw codeError }
        return "the-code"
    }
    func exchange(code: String, verifier: String, redirectURI: String) async throws -> String {
        if let exchangeError { throw exchangeError }
        #expect(code == "the-code" && verifier.count == 43 && redirectURI.hasPrefix("http://127.0.0.1:"))
        return "refresh-new"
    }
    func revoke(refreshToken: String) async { lock.withLock { revoked.append(refreshToken) } }
}

/// The consent page: "the browser" calls the receiver's redirect with the URL's state.
@MainActor
private final class FakePresenter: GooglePhotosAuthPresenter {
    var deliver: ((URL) -> Void)?
    var closes = false
    private(set) var presented: URL?
    private(set) var dismissed = false
    func present(_ url: URL, onClose: @escaping @Sendable () -> Void) {
        presented = url
        if closes { onClose() } else { deliver?(url) }
    }
    func dismiss() { dismissed = true }
}

private final class FakeReceiver: LoopbackReceiving, @unchecked Sendable {
    private let lock = NSLock()
    private var targets: [String] = []
    private var stopped = false
    func start() async throws -> UInt16 { 50_123 }
    func deliver(_ t: String) { lock.withLock { targets.append(t) } }
    func target() async throws -> String {
        while true {
            if let t = lock.withLock({ targets.isEmpty ? nil : targets.removeFirst() }) { return t }
            if lock.withLock({ stopped }) { throw CancellationError() }
            try await Task.sleep(for: .milliseconds(2))
        }
    }
    func stop() { lock.withLock { stopped = true } }
}

private func state(of url: URL) -> String {
    URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "state" }?.value ?? ""
}

private func tempDir() throws -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("ds-gphotos-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

// MARK: - PKCE, settings, values

@Suite struct GooglePhotosBasicsTests {
    @Test func pkceMatchesTheRFCVector() {
        let p = PKCE(verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")
        #expect(p.challenge == "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")
        let r = PKCE()
        #expect(r.verifier.count == 43)
        #expect(!r.verifier.contains("=") && !r.verifier.contains("+") && !r.verifier.contains("/"))
        #expect(PKCE().verifier != r.verifier)
    }

    @MainActor @Test func settingsKeepSecretsInTheKeychainItems() {
        let (s, secret, refresh) = settings(clientID: "  id  ")
        #expect(s.clientID == "id")
        #expect(s.defaults.string(forKey: GooglePhotosSettings.clientIDKey) == "id")
        #expect(s.clientSecret == "sec" && s.refreshToken == "refresh-1")
        #expect(s.isConfigured && s.isSignedIn)
        _ = refresh.item.write("  ")
        #expect(s.refreshToken == nil && !s.isSignedIn)
        _ = secret.item.write(nil)
        #expect(s.clientSecret == nil)
        s.clientIDOverride = " "
        #expect(s.defaults.object(forKey: GooglePhotosSettings.clientIDKey) == nil)
        #expect(!s.isConfigured)
        // No secret or token ever lands in the preferences.
        let all = s.defaults.dictionaryRepresentation().values.compactMap { $0 as? String }
        #expect(!all.contains("sec") && !all.contains("refresh-1"))
    }

    @Test func builtInClientComesFromInfoPlist() {
        #expect(GooglePhotosClient.builtIn(nil) == nil)
        #expect(GooglePhotosClient.builtIn(["DSGoogleClientID": "  "]) == nil)
        #expect(GooglePhotosClient.builtIn(["DSGoogleClientID": "1-a.apps.googleusercontent.com"])
            == GooglePhotosClient(id: "1-a.apps.googleusercontent.com"))
        #expect(GooglePhotosClient.builtIn(["DSGoogleClientID": "x", "DSGoogleClientSecret": " s "])
            == GooglePhotosClient(id: "x", secret: "s"))
        // The test runner's bundle has none built in.
        #expect(GooglePhotosClient.builtIn(Bundle.main.infoDictionary) == nil)
    }

    @MainActor @Test func ownClientWinsOverTheBuiltInOne() {
        let (s, _, _) = settings(clientID: "", secret: "mine", builtIn: GooglePhotosClient(id: "app-id", secret: "app-secret"))
        #expect(s.clientID == "app-id" && s.clientSecret == "app-secret" && s.usesBuiltInClient && s.isSignedIn)
        s.clientIDOverride = "own-id"
        #expect(s.clientID == "own-id" && s.clientSecret == "mine" && !s.usesBuiltInClient)
    }

    @Test func uploadSummariesAndErrors() {
        let u = URL(fileURLWithPath: "/x/a.jpg")
        #expect(GooglePhotosUpload(albumTitle: "Trip", uploaded: [u, u]).summary == "2 uploaded to Google Photos (Trip)")
        let mixed = GooglePhotosUpload(uploaded: [u], failed: [GooglePhotosFailure(url: u, reason: "x")], cancelled: true,
                                       signInNeeded: true)
        #expect(mixed.summary == "1 uploaded to Google Photos · 1 failed · stopped · sign in again in Settings ▸ Google Photos")
        #expect(GooglePhotosUpload(error: "no album").summary == "Google Photos: no album")
        #expect(GooglePhotosTarget(albumTitle: "Trip").summary == "Google Photos (Trip)")
        for e in [GooglePhotosError.notSetUp, .notSignedIn, .keychain, .failed("boom")] {
            #expect(!(e.errorDescription ?? "").isEmpty)
        }
        #expect(GooglePhotosError.failed("boom").errorDescription == "boom")
    }

    @Test func recipesKeepTheirTargetAndOldRecipesDecode() throws {
        var r = ExportRecipe(name: "Web", format: .jpeg, quality: 82)
        r.googlePhotos = GooglePhotosTarget(albumID: "a1", albumTitle: "Trip")
        #expect(r.summary.hasSuffix(" · Google Photos (Trip)"))
        let back = try JSONDecoder().decode(ExportRecipe.self, from: JSONEncoder().encode(r))
        #expect(back == r)
        var old = try JSONSerialization.jsonObject(with: JSONEncoder().encode(r)) as! [String: Any]
        old.removeValue(forKey: "googlePhotos")
        let decoded = try JSONDecoder().decode(ExportRecipe.self, from: JSONSerialization.data(withJSONObject: old))
        #expect(decoded.googlePhotos == nil)
        #expect(ExportRecipe.Format.allCases.filter(\.uploadsToGooglePhotos) == [.jpeg, .heic])
    }
}

// MARK: - Loopback receiver and sign-in

@Suite struct GooglePhotosSignInTests {
    @Test func requestLineParsing() {
        #expect(LoopbackReceiver.requestTarget(Data("GET /?code=a&state=b HTTP/1.1\r\nHost: x\r\n".utf8)) == "/?code=a&state=b")
        #expect(LoopbackReceiver.requestTarget(Data("GET /?code=a".utf8)) == nil, "incomplete line")
        #expect(LoopbackReceiver.requestTarget(Data("garbage\r\n".utf8)) == "")
        #expect(LoopbackReceiver.isRedirect("/?code=a"))
        #expect(!LoopbackReceiver.isRedirect("/favicon.ico"))
    }

    @Test func loopbackReceivesTheRedirect() async throws {
        let r = try LoopbackReceiver()
        let port = try await r.start()
        #expect(port > 0)
        let base = "http://127.0.0.1:\(port)"
        let session = URLSession(configuration: .ephemeral)
        let (_, favicon) = try await session.data(from: URL(string: "\(base)/favicon.ico")!)
        #expect((favicon as? HTTPURLResponse)?.statusCode == 404)
        let (body, ok) = try await session.data(from: URL(string: "\(base)/?code=4%2F0A&state=s1")!)
        #expect((ok as? HTTPURLResponse)?.statusCode == 200)
        #expect(String(decoding: body, as: UTF8.self).contains("close this window"))
        #expect(try await r.target() == "/?code=4%2F0A&state=s1")
        r.stop()
        await #expect(throws: CancellationError.self) { try await r.target() }
    }

    @Test func waitingReceiverGetsTheRedirectAndCancelStopsIt() async throws {
        let r = try LoopbackReceiver()
        let port = try await r.start()
        async let got = r.target()
        try await Task.sleep(for: .milliseconds(20))
        _ = try await URLSession(configuration: .ephemeral).data(from: URL(string: "http://127.0.0.1:\(port)/?code=c&state=s")!)
        #expect(try await got == "/?code=c&state=s")
        let waiting = Task { try await r.target() }
        try await Task.sleep(for: .milliseconds(20))
        waiting.cancel()
        await #expect(throws: CancellationError.self) { try await waiting.value }
    }

    @Test func loopbackClosesIdleConnectionsAndCapsItsQueue() async throws {
        let r = try LoopbackReceiver(idleTimeout: 0.2)
        let port = try await r.start()
        // A client that connects and never sends anything is cut off.
        let c = NWConnection(host: "127.0.0.1", port: NWEndpoint.Port(rawValue: port)!, using: .tcp)
        let closed = GPBox(false)
        c.start(queue: .global())
        c.receive(minimumIncompleteLength: 1, maximumLength: 1) { _, _, done, error in
            closed.mutate { $0 = done || error != nil }
        }
        let start = ContinuousClock.now
        while !closed.current && ContinuousClock.now - start < .seconds(5) { try await Task.sleep(for: .milliseconds(20)) }
        #expect(closed.current, "the idle connection was closed")
        c.cancel()
        // Only the first `maxQueued` untaken redirects are kept.
        let session = URLSession(configuration: .ephemeral)
        for i in 0..<(LoopbackReceiver.maxQueued + 4) {
            _ = try await session.data(from: URL(string: "http://127.0.0.1:\(port)/?n=\(i)")!)
        }
        for i in 0..<LoopbackReceiver.maxQueued { #expect(try await r.target() == "/?n=\(i)") }
        let next = Task { try await r.target() }
        try await Task.sleep(for: .milliseconds(50))
        r.stop()
        await #expect(throws: CancellationError.self) { try await next.value }
    }

    @MainActor @Test func signInStoresTheRefreshToken() async throws {
        let (s, _, refresh) = settings(refresh: nil)
        let google = FakeGooglePhotos(), presenter = FakePresenter(), receiver = FakeReceiver()
        presenter.deliver = { url in receiver.deliver("/?code=the-code&state=\(state(of: url))") }
        try await GooglePhotosSignIn(settings: s, service: google, presenter: presenter) { receiver }.run()
        #expect(refresh.current == "refresh-new")
        #expect(presenter.presented?.absoluteString.contains("redirect=http://127.0.0.1:50123/") == true)
        #expect(presenter.dismissed)
    }

    @MainActor @Test func signInFailures() async throws {
        let google = FakeGooglePhotos(), receiver = FakeReceiver()
        // Not set up.
        let (bare, _, _) = settings(clientID: "", refresh: nil)
        await #expect(throws: GooglePhotosError.notSetUp) {
            try await GooglePhotosSignIn(settings: bare, service: google, presenter: FakePresenter()) { receiver }.run()
        }
        // The user closes the window.
        let (s, _, refresh) = settings(refresh: nil)
        let closing = FakePresenter()
        closing.closes = true
        await #expect(throws: CancellationError.self) {
            try await GooglePhotosSignIn(settings: s, service: google, presenter: closing) { FakeReceiver() }.run()
        }
        // A forged redirect (another local client) is ignored; the real one still signs in.
        let (s2, _, refresh2) = settings(refresh: nil)
        let forged = FakePresenter(), r2 = FakeReceiver()
        forged.deliver = { url in
            r2.deliver("/?code=x&state=other")
            r2.deliver("/?x")
            r2.deliver("/?code=the-code&state=\(state(of: url))")
        }
        try await GooglePhotosSignIn(settings: s2, service: google, presenter: forged) { r2 }.run()
        #expect(refresh2.current == "refresh-new")
        // Google's own answer (our state) ends the sign-in, a refusal included.
        let denied = FakePresenter(), r4 = FakeReceiver()
        denied.deliver = { url in r4.deliver("/?error=access_denied&state=\(state(of: url))") }
        let failing = FakeGooglePhotos()
        failing.codeError = GooglePhotosError.failed("access_denied")
        await #expect(throws: GooglePhotosError.self) {
            try await GooglePhotosSignIn(settings: s, service: failing, presenter: denied) { r4 }.run()
        }
        // The Keychain refuses the token.
        refresh.refuse = true
        let ok = FakePresenter(), r3 = FakeReceiver()
        ok.deliver = { url in r3.deliver("/?code=the-code&state=\(state(of: url))") }
        await #expect(throws: GooglePhotosError.keychain) {
            try await GooglePhotosSignIn(settings: s, service: google, presenter: ok) { r3 }.run()
        }
        #expect(refresh.current == nil)
    }
}

// MARK: - Settings model

@Suite @MainActor struct GooglePhotosSettingsModelTests {
    @Test func clientIsSavedThenRemoved() {
        let (s, secret, _) = settings(clientID: "", secret: nil, refresh: nil)
        let m = GooglePhotosSettingsModel(settings: s, service: FakeGooglePhotos()) {}
        #expect(m.status == "Not set up" && !m.canSaveClient && !m.canSignIn)
        m.clientIDEntry = " 123.apps.googleusercontent.com "
        m.secretEntry = " GOCSPX-x "
        #expect(m.canSaveClient)
        m.saveClient()
        #expect(m.clientID == "123.apps.googleusercontent.com" && m.hasSecret && m.secretEntry.isEmpty)
        #expect(secret.current == "GOCSPX-x")
        #expect(m.message == "Saved.")
        #expect(m.status == "Signed out" && m.canSignIn)
        m.clientIDEntry = "odd"
        m.saveClient()
        #expect(m.message?.contains("usually end") == true)
        secret.refuse = true
        m.secretEntry = "new"
        m.saveClient()
        #expect(m.message == "The Keychain refused the client secret.")
        secret.refuse = false
        m.removeClient()
        #expect(m.clientID.isEmpty && !m.hasSecret && secret.current == nil && s.clientID.isEmpty)
    }

    @Test func builtInClientNeedsNoSetup() {
        let (s, _, _) = settings(clientID: "", secret: nil, refresh: nil, builtIn: GooglePhotosClient(id: "app-id"))
        let m = GooglePhotosSettingsModel(settings: s, service: FakeGooglePhotos()) {}
        #expect(m.hasBuiltInClient && m.canSignIn && m.status == "Signed out")
        #expect(m.clientID == "app-id" && m.ownClientID.isEmpty && !m.hasSecret)
        m.clientIDEntry = "own.apps.googleusercontent.com"
        m.saveClient()
        #expect(m.clientID == "own.apps.googleusercontent.com" && m.ownClientID == m.clientID)
        m.removeClient()
        #expect(m.clientID == "app-id" && m.message?.contains("Darkslide's own") == true)
    }

    @Test func signInAndOut() async {
        let (s, _, refresh) = settings(refresh: nil)
        let google = FakeGooglePhotos()
        let m = GooglePhotosSettingsModel(settings: s, service: google) { _ = refresh.item.write("tok") }
        await m.signInNow()
        #expect(m.isSignedIn && m.status == "Signed in")
        #expect(m.message?.hasPrefix("Signed in") == true)
        #expect(!m.canSaveClient, "the client is fixed while signed in")
        m.removeClient()
        #expect(!m.clientID.isEmpty)
        await m.signInNow() // already signed in: nothing happens
        await m.signOut()
        #expect(!m.isSignedIn && refresh.current == nil && google.revoked == ["tok"])
        #expect(m.message == "Signed out.")
        await m.signOut()
        #expect(google.revoked.count == 1)
    }

    @Test func signInCancelAndFailureAreExplained() async {
        let (s, _, _) = settings(refresh: nil)
        let cancelled = GooglePhotosSettingsModel(settings: s, service: FakeGooglePhotos()) { throw CancellationError() }
        await cancelled.signInNow()
        #expect(cancelled.message == "Sign-in cancelled." && !cancelled.isSignedIn && !cancelled.busy)
        let failed = GooglePhotosSettingsModel(settings: s, service: FakeGooglePhotos()) {
            throw GooglePhotosError.failed("invalid_client")
        }
        await failed.signInNow()
        #expect(failed.message == "invalid_client")
        #expect(GooglePhotosSettingsModel.setupSteps.count == 4)
    }
}

// MARK: - The live service over a fake Google (through the Rust core)

/// Google's token endpoint and Library API, just enough for one run.
private final class FakeGoogleHTTP: GphotosHttp, @unchecked Sendable {
    private let lock = NSLock()
    private(set) var requests: [GphotosHttpRequest] = []
    var revokedOK = true
    private var uploaded: [String: Data] = [:]
    private var sessions: [String: Data] = [:]
    private(set) var created: [(name: String, desc: String?, album: String?)] = []

    private func json(_ v: Any, status: UInt16 = 200) -> GphotosHttpResponse {
        GphotosHttpResponse(status: status, headers: [], body: try! JSONSerialization.data(withJSONObject: v))
    }

    func send(request r: GphotosHttpRequest) throws -> GphotosHttpResponse {
        try lock.withLock {
            requests.append(r)
            let form = String(decoding: r.body, as: UTF8.self)
            switch r.url {
            case "https://oauth2.googleapis.com/token":
                if form.contains("grant_type=authorization_code") {
                    return json(["access_token": "a1", "expires_in": 3599, "refresh_token": "refresh-new",
                                 "scope": "https://www.googleapis.com/auth/photoslibrary.appendonly"])
                }
                return form.contains("refresh_token=refresh-1") ? json(["access_token": "a1", "expires_in": 3599])
                    : json(["error": "invalid_grant"], status: 400)
            case "https://oauth2.googleapis.com/revoke":
                return GphotosHttpResponse(status: revokedOK ? 200 : 500, headers: [], body: Data())
            case "https://photoslibrary.googleapis.com/v1/albums?pageSize=50":
                return json(["albums": [["id": "al1", "title": "Trip", "mediaItemsCount": "3"]]])
            case "https://photoslibrary.googleapis.com/v1/albums":
                return json(["id": "al2", "title": "New"])
            case "https://photoslibrary.googleapis.com/v1/uploads":
                let id = "s\(sessions.count)"
                sessions[id] = Data()
                return GphotosHttpResponse(status: 200, headers: [
                    GphotosHeader(name: "X-Goog-Upload-URL", value: "https://photoslibrary.googleapis.com/mock-upload/\(id)"),
                    GphotosHeader(name: "X-Goog-Upload-Chunk-Granularity", value: "262144"),
                ], body: Data())
            case let u where u.hasPrefix("https://photoslibrary.googleapis.com/mock-upload/"):
                let id = String(u.dropFirst("https://photoslibrary.googleapis.com/mock-upload/".count))
                sessions[id, default: Data()].append(r.body)
                let token = "tok-\(id)"
                uploaded[token] = sessions[id]
                return GphotosHttpResponse(status: 200, headers: [], body: Data(token.utf8))
            case "https://photoslibrary.googleapis.com/v1/mediaItems:batchCreate":
                let body = try JSONSerialization.jsonObject(with: r.body) as! [String: Any]
                let items = body["newMediaItems"] as! [[String: Any]]
                var results: [[String: Any]] = []
                for it in items {
                    let simple = it["simpleMediaItem"] as! [String: String]
                    let name = simple["fileName"]!, token = simple["uploadToken"]!
                    if name.hasPrefix("bad") {
                        results.append(["uploadToken": token, "status": ["code": 3, "message": "Failed"]])
                    } else {
                        created.append((name, it["description"] as? String, body["albumId"] as? String))
                        results.append(["uploadToken": token, "status": ["message": "Success"],
                                        "mediaItem": ["id": "m-\(name)"]])
                    }
                }
                return json(["newMediaItemResults": results], status: 207)
            default:
                return json(["error": ["message": "not found"]], status: 404)
            }
        }
    }
}

@Suite struct RustGooglePhotosServiceTests {
    @MainActor @Test func albumsAndUploadGoThroughTheRustCore() async throws {
        let (s, _, _) = settings()
        let http = FakeGoogleHTTP()
        let service = RustGooglePhotosService(settings: s, http: http)
        #expect(service.isSignedIn)
        let albums = try await service.albums()
        #expect(albums == [GooglePhotosAlbum(id: "al1", title: "Trip", itemCount: 3)])
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let good = dir.appendingPathComponent("a.jpg"), bad = dir.appendingPathComponent("bad.heic")
        try Data("jpegbytes".utf8).write(to: good)
        try Data("heic".utf8).write(to: bad)
        let seen = GPBox<[GooglePhotosProgress]>([])
        let r = try await service.upload([GooglePhotosFile(url: good, description: "Sunset"), GooglePhotosFile(url: bad)],
                                         to: GooglePhotosTarget(albumTitle: "New")) { p in seen.mutate { $0.append(p) } }
        #expect(r.albumTitle == "New")
        #expect(r.uploaded.map(\.lastPathComponent) == ["a.jpg"])
        #expect(r.failed.map(\.url.lastPathComponent) == ["bad.heic"] && r.failed[0].reason == "Failed")
        #expect(!r.signInNeeded && r.error == nil && !r.cancelled)
        #expect(seen.current.contains { $0.adding } && seen.current.allSatisfy { $0.total == 2 })
        #expect(http.created.first?.desc == "Sunset" && http.created.first?.album == "al2")
        // Existing album: no album lookup.
        let before = http.requests.count
        _ = try await service.upload([GooglePhotosFile(url: good)], to: GooglePhotosTarget(albumID: "al1", albumTitle: "Trip")) { _ in }
        #expect(!http.requests[before...].contains { $0.url.contains("/v1/albums") })
        #expect(http.created.last?.album == "al1")
    }

    @MainActor @Test func signInStepsAndSignOut() async throws {
        let (s, _, _) = settings(refresh: nil)
        let http = FakeGoogleHTTP()
        let service = RustGooglePhotosService(settings: s, http: http)
        let url = try service.authorizationURL(redirectURI: "http://127.0.0.1:9/", challenge: "ch", state: "st")
        #expect(url.host == "accounts.google.com" && url.query?.contains("code_challenge=ch") == true)
        #expect(try service.code(fromRedirect: "/?code=abc&state=st", state: "st") == "abc")
        #expect(throws: GooglePhotosError.self) { try service.code(fromRedirect: "/?code=abc&state=no", state: "st") }
        #expect(try await service.exchange(code: "abc", verifier: "v", redirectURI: "http://127.0.0.1:9/") == "refresh-new")
        let form = String(decoding: http.requests.last!.body, as: UTF8.self)
        #expect(form.contains("client_secret=sec") && form.contains("code_verifier=v"))
        await service.revoke(refreshToken: "refresh-new")
        http.revokedOK = false
        await service.revoke(refreshToken: "refresh-new") // best effort: no throw
        #expect(http.requests.last?.url == "https://oauth2.googleapis.com/revoke")
    }

    @MainActor @Test func errorsInWords() async throws {
        let (bare, _, _) = settings(clientID: "", refresh: nil)
        await #expect(throws: GooglePhotosError.notSetUp) { try await RustGooglePhotosService(settings: bare).albums() }
        #expect(throws: GooglePhotosError.self) {
            try RustGooglePhotosService(settings: bare).authorizationURL(redirectURI: "r", challenge: "c", state: "s")
        }
        let (out, _, _) = settings(refresh: nil)
        await #expect(throws: GooglePhotosError.notSignedIn) {
            try await RustGooglePhotosService(settings: out).upload([], to: GooglePhotosTarget()) { _ in }
        }
        // A revoked token: the album lookup fails and says to sign in.
        let (s, _, refresh) = settings()
        _ = refresh.item.write("revoked")
        let http = FakeGoogleHTTP()
        let service = RustGooglePhotosService(settings: s, http: http)
        let r = try await service.upload([], to: GooglePhotosTarget(albumTitle: "X")) { _ in }
        #expect(r.signInNeeded && r.error?.contains("sign-in needed") == true)
        do {
            _ = try await service.albums()
            Issue.record("expected a failure")
        } catch {
            #expect((error as? LocalizedError)?.errorDescription?.contains("sign-in needed") == true)
        }
        #expect(RustGooglePhotosService.error(.Cancelled) is CancellationError)
    }
}

// MARK: - URLSession transport

private final class StubProtocol: URLProtocol {
    nonisolated(unsafe) static var answer: (URLRequest) -> (Int, [String: String], Data)? = { _ in nil }
    nonisolated(unsafe) static var seen: [URLRequest] = []
    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func startLoading() {
        Self.seen.append(request)
        guard let (status, headers, body) = Self.answer(request) else {
            client?.urlProtocol(self, didFailWithError: URLError(.notConnectedToInternet))
            return
        }
        let r = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1", headerFields: headers)!
        client?.urlProtocol(self, didReceive: r, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: body)
        client?.urlProtocolDidFinishLoading(self)
    }
    override func stopLoading() {}
}

@Suite(.serialized) struct URLSessionHTTPTests {
    private func http() -> URLSessionHTTP {
        let c = URLSessionConfiguration.ephemeral
        c.protocolClasses = [StubProtocol.self]
        return URLSessionHTTP(session: URLSession(configuration: c))
    }

    @Test func sendsAndReturnsTheAnswer() throws {
        StubProtocol.seen = []
        StubProtocol.answer = { _ in (207, ["X-Goog-Upload-URL": "https://u"], Data("ok".utf8)) }
        let r = try http().send(request: GphotosHttpRequest(
            method: "POST", url: "https://photoslibrary.googleapis.com/v1/uploads",
            headers: [GphotosHeader(name: "X-Goog-Upload-Command", value: "start")], body: Data("b".utf8)))
        #expect(r.status == 207 && r.body == Data("ok".utf8))
        #expect(r.headers.contains { $0.name.lowercased() == "x-goog-upload-url" && $0.value == "https://u" })
        let sent = try #require(StubProtocol.seen.last)
        #expect(sent.httpMethod == "POST" && sent.value(forHTTPHeaderField: "X-Goog-Upload-Command") == "start")
        _ = try http().send(request: GphotosHttpRequest(method: "GET", url: "https://x.example/a", headers: [], body: Data()))
        #expect(StubProtocol.seen.last?.httpMethod == "GET")
    }

    @Test func noAnswerAndBadURLsThrow() {
        StubProtocol.answer = { _ in nil }
        #expect(throws: DsError.self) {
            try http().send(request: GphotosHttpRequest(method: "GET", url: "https://x.example/", headers: [], body: Data()))
        }
        #expect(throws: DsError.self) {
            try http().send(request: GphotosHttpRequest(method: "GET", url: "", headers: [], body: Data()))
        }
    }

    @Test func onlyHTTPSIsSentAndRedirectsAreRefused() throws {
        StubProtocol.seen = []
        StubProtocol.answer = { _ in (200, [:], Data()) }
        for url in ["http://photoslibrary.googleapis.com/v1/albums", "file:///etc/hosts", "photoslibrary.googleapis.com"] {
            #expect(throws: DsError.self) {
                try http().send(request: GphotosHttpRequest(method: "GET", url: url, headers: [], body: Data()))
            }
        }
        #expect(StubProtocol.seen.isEmpty, "nothing went out")
        let task = URLSession.shared.dataTask(with: URL(string: "https://x.example/")!)
        let redirect = HTTPURLResponse(url: URL(string: "https://x.example/")!, statusCode: 302, httpVersion: nil,
                                       headerFields: ["Location": "https://elsewhere.example/"])!
        let followed = GPBox<URLRequest??>(nil)
        NoRedirects.shared.urlSession(URLSession.shared, task: task, willPerformHTTPRedirection: redirect,
                                      newRequest: URLRequest(url: URL(string: "https://elsewhere.example/")!)) { r in
            followed.mutate { $0 = .some(r) }
        }
        #expect(followed.current == .some(nil), "the redirect is answered with nil: not followed")
    }
}

// MARK: - Export sheet

private final class GPBox<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    func mutate(_ f: (inout T) -> Void) { lock.withLock { f(&value) } }
    var current: T { lock.withLock { value } }
}

@Suite @MainActor struct GooglePhotosExportTests {
    private func model(_ google: FakeGooglePhotos?, recipes: [ExportRecipe], export: Exp6FakeExport = Exp6FakeExport(),
                       items: [ExportJobItem]) -> ExportModel {
        let store = ExportRecipeStore(defaults: testDefaults())
        store.save(recipes)
        return ExportModel(items: items, defaultFolder: URL(fileURLWithPath: "/tmp/out"), service: export, store: store,
                           googlePhotos: google)
    }

    private func item(_ id: Int64, caption: String = "", title: String = "") -> ExportJobItem {
        var m = IPTCMetadata()
        m.caption = caption
        m.title = title
        return ExportJobItem(photoID: PhotoID(id), url: URL(fileURLWithPath: "/s/\(id).ARW"), seq: Int(id), metadata: m)
    }

    private func uploadingRecipe(_ format: ExportRecipe.Format = .jpeg, album: String = "Trip") -> ExportRecipe {
        var r = ExportRecipe(name: "Web", format: format)
        r.googlePhotos = GooglePhotosTarget(albumTitle: album)
        return r
    }

    @Test func problemsBeforeExport() {
        let google = FakeGooglePhotos()
        #expect(model(google, recipes: [uploadingRecipe()], items: [item(1)]).problem == nil)
        #expect(model(google, recipes: [uploadingRecipe(.tiff16)], items: [item(1)]).problem?.contains("JPEG or HEIC") == true)
        #expect(model(google, recipes: [uploadingRecipe(album: " ")], items: [item(1)]).problem?.contains("Name the") == true)
        google.signedIn = false
        #expect(model(google, recipes: [uploadingRecipe()], items: [item(1)]).problem?.contains("Sign in") == true)
        #expect(model(nil, recipes: [uploadingRecipe()], items: [item(1)]).problem?.contains("Sign in") == true)
        var off = uploadingRecipe()
        off.enabled = false
        #expect(model(nil, recipes: [off, ExportRecipe(name: "Plain")], items: [item(1)]).problem == nil)
    }

    @Test func jobsGroupWrittenFilesByRecipeWithCaptions() {
        let web = uploadingRecipe(), plain = ExportRecipe(name: "Plain")
        var other = uploadingRecipe(.heic, album: "Other")
        other.name = "Other"
        let items = [item(1, caption: " Sunset ", title: "T1"), item(2, title: "Title only"), item(3)]
        func w(_ id: Int64, _ recipe: String) -> ExportedOutput {
            ExportedOutput(photoID: PhotoID(id), recipeName: recipe, url: URL(fileURLWithPath: "/o/\(recipe)-\(id).jpg"),
                           width: 1, height: 1, bytes: 1)
        }
        let outcome = ExportOutcome(written: [w(1, "Web"), w(2, "Web"), w(3, "Web"), w(1, "Plain")], failures: [],
                                    cancelled: false, jpegEncoder: "")
        let jobs = ExportModel.googlePhotosJobs(outcome, job: ExportJob(items: items, recipes: [web, plain, other],
                                                                        defaultFolder: URL(fileURLWithPath: "/o")))
        #expect(jobs.count == 1, "Other wrote nothing; Plain doesn't upload")
        #expect(jobs[0].target.albumTitle == "Trip")
        #expect(jobs[0].files.map(\.description) == ["Sunset", "Title only", nil])
    }

    @Test func exportThenUploadWithProgressAndResults() async throws {
        let google = FakeGooglePhotos()
        google.failNames = ["Web-2.jpg"]
        let (gate, open) = AsyncStream.makeStream(of: Void.self)
        google.gate = gate
        let export = Exp6FakeExport()
        export.outcome = ExportOutcome(written: [1, 2].map {
            ExportedOutput(photoID: PhotoID($0), recipeName: "Web", url: URL(fileURLWithPath: "/o/Web-\($0).jpg"),
                           width: 1, height: 1, bytes: 1)
        }, failures: [], cancelled: false, jpegEncoder: "")
        let m = model(google, recipes: [uploadingRecipe()], export: export, items: [item(1, caption: "C"), item(2)])
        m.start()
        while m.uploading == nil { try await Task.sleep(for: .milliseconds(2)) }
        #expect(m.runningText == "Uploading 1 of 2 to Google Photos…")
        open.yield()
        await m.waitUntilDone()
        guard case .finished = m.phase else { Issue.record("not finished: \(m.phase)"); return }
        #expect(m.uploading == nil)
        #expect(m.googleUploads.count == 1)
        let u = m.googleUploads[0]
        #expect(u.recipeName == "Web" && u.albumTitle == "Trip")
        #expect(u.uploaded.count == 1 && u.failed.map(\.url.lastPathComponent) == ["Web-2.jpg"])
        #expect(google.uploads.first?.0.first?.description == "C")
        m.reset()
        #expect(m.googleUploads.isEmpty)
    }

    @Test func uploadErrorsAndCancelledExports() async throws {
        let google = FakeGooglePhotos()
        google.uploadError = GooglePhotosError.failed("quota")
        let export = Exp6FakeExport()
        export.outcome = ExportOutcome(written: [ExportedOutput(photoID: PhotoID(1), recipeName: "Web",
                                                                url: URL(fileURLWithPath: "/o/a.jpg"), width: 1, height: 1, bytes: 1)],
                                       failures: [], cancelled: false, jpegEncoder: "")
        let m = model(google, recipes: [uploadingRecipe()], export: export, items: [item(1)])
        m.start()
        await m.waitUntilDone()
        #expect(m.googleUploads.first?.error == "quota")
        google.uploadError = CancellationError()
        m.reset()
        m.start()
        await m.waitUntilDone()
        #expect(m.googleUploads.first?.cancelled == true)
        // A cancelled export uploads nothing.
        export.outcome.cancelled = true
        google.uploadError = nil
        let before = google.uploads.count
        m.reset()
        m.start()
        await m.waitUntilDone()
        #expect(google.uploads.count == before && m.googleUploads.isEmpty)
    }

    @Test func albumsAreListedSortedOrExplained() async {
        let google = FakeGooglePhotos()
        let m = model(google, recipes: [uploadingRecipe()], items: [item(1)])
        await m.loadGoogleAlbums()
        #expect(m.googleAlbums.map(\.title) == ["alpha", "Beta"])
        google.albumsError = GooglePhotosError.failed("offline")
        await m.loadGoogleAlbums()
        #expect(m.googleAlbumsMessage == "offline")
        google.albumsError = CancellationError()
        await m.loadGoogleAlbums()
        #expect(m.googleAlbumsMessage == "offline", "a cancelled load changes nothing")
        google.signedIn = false
        google.albumsError = nil
        await m.loadGoogleAlbums()
        #expect(m.googleAlbumsMessage == "offline")
    }
}
