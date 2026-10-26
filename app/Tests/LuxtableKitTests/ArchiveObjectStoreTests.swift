import LuxtableFFI
import Foundation
import Security
import Testing
@testable import LuxtableCore

/// A Keychain item kept in memory.
private final class MemoryItem: @unchecked Sendable {
    private let lock = NSLock()
    private var value: String?
    var refuse = false
    /// The next reads that fail (a locked Keychain, access denied).
    var failingReads = 0
    private(set) var reads = 0
    init(_ v: String? = nil) { value = v }
    var item: KeychainItem {
        let lookup: @Sendable () -> KeychainLookup = {
            self.lock.withLock {
                self.reads += 1
                if self.failingReads > 0 {
                    self.failingReads -= 1
                    return .failed
                }
                return self.value.map(KeychainLookup.found) ?? .notFound
            }
        }
        return KeychainItem(read: { lookup().value },
                            write: { v in self.lock.withLock { if self.refuse { return false }; self.value = v; return true } },
                            lookup: lookup)
    }
    var current: String? { lock.withLock { value } }
}

private func keys(_ id: String? = nil, _ secret: String? = nil) -> (ArchiveObjectKeys, MemoryItem, MemoryItem) {
    let a = MemoryItem(id), s = MemoryItem(secret)
    return (ArchiveObjectKeys(accessKeyID: a.item, secret: s.item), a, s)
}

private func store(bucket: String = "photos", prefix: String = "luxtable/", endpoint: String = "https://s3.example.test",
                   pathStyle: Bool = false) -> ObjectStoreSettings {
    ObjectStoreSettings(endpoint: endpoint, region: "us-east-1", bucket: bucket, prefix: prefix, pathStyle: pathStyle)
}

@MainActor
@Suite struct ArchiveObjectStoreTests {
    @Test func destinationAndObjectSettingsPersistAndReachTheSnapshot() {
        let suite = testSuiteName()
        let a = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(a.archiveDestination == .folder)
        #expect(a.archiveObjectStore.prefix == "luxtable/", "a sensible default path")
        #expect(a.archiveSnapshot.objectStore == nil)
        a.archiveTarget = "/Volumes/NAS/Photos"
        a.archiveDestination = .object
        a.archiveObjectStore = store(bucket: "", pathStyle: true)
        let b = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(b.archiveDestination == .object)
        #expect(b.archiveObjectStore == store(bucket: "", pathStyle: true))
        // Object storage needs a bucket; the folder target doesn't count.
        #expect(b.archiveSnapshot.objectStore != nil && !b.archiveSnapshot.isActive)
        #expect(ArchiveDisplay.offText(b.archiveSnapshot) == "Archive off: no bucket set")
        b.archiveObjectStore = store()
        #expect(b.archiveSnapshot.isActive && b.archiveSnapshot.hasTarget)
        b.archiveDestination = .folder
        #expect(b.archiveSnapshot.objectStore == nil && b.archiveSnapshot.isActive)
        #expect(ArchiveDestinationKind.folder.title == "Folder" && ArchiveDestinationKind.object.title == "Object storage")
    }

    @Test func keysLiveOnlyInTheKeychainAndAreCached() {
        let (k, id, secret) = keys()
        #expect(k.credentials == nil && k.accessKeyID == nil && !k.hasSecret)
        #expect(k.save(accessKeyID: "  AKID  ", secret: " s3cr3t "))
        #expect(id.current == "AKID" && secret.current == "s3cr3t")
        #expect(k.credentials == ObjectCredentials(accessKeyId: "AKID", secretAccessKey: "s3cr3t"))
        let reads = secret.reads
        _ = k.credentials
        _ = k.credentials
        #expect(secret.reads == reads, "cached: no Keychain read per tick")
        // A new ID without a new secret keeps the saved secret.
        #expect(k.save(accessKeyID: "AKID2", secret: ""))
        #expect(k.credentials?.accessKeyId == "AKID2" && k.credentials?.secretAccessKey == "s3cr3t")
        // An ID alone (no secret anywhere) isn't a key pair, but the ID is shown.
        let (half, _, _) = keys("only-id", nil)
        #expect(half.credentials == nil && half.accessKeyID == "only-id")
        #expect(k.remove() && k.credentials == nil && id.current == nil && secret.current == nil)
        id.refuse = true
        #expect(!k.save(accessKeyID: "x", secret: "y"))
        id.refuse = false
        secret.refuse = true
        #expect(!k.save(accessKeyID: "x", secret: "y"))
    }

    @Test func aFailedKeychainReadIsNotCached() {
        let (k, _, secret) = keys("AKID", "s3cr3t")
        secret.failingReads = 1
        #expect(k.credentials == nil, "the Keychain couldn't be read this time")
        #expect(k.credentials == ObjectCredentials(accessKeyId: "AKID", secretAccessKey: "s3cr3t"), "asked again")
        let reads = secret.reads
        _ = k.credentials
        #expect(secret.reads == reads, "a good read is cached")
        // A definite "no such item" is cached too.
        let (none, _, noSecret) = keys("AKID", nil)
        #expect(none.credentials == nil)
        let before = noSecret.reads
        #expect(none.credentials == nil && noSecret.reads == before)
        // The ID failing is no answer either.
        let (k2, id2, _) = keys("AKID", "s3cr3t")
        id2.failingReads = 1
        #expect(k2.credentials == nil)
        #expect(k2.credentials != nil)
    }

    @Test func removingKeysReportsAKeychainThatRefuses() {
        let (k, id, secret) = keys("AKID", "s3cr3t")
        id.refuse = true
        #expect(!k.remove(), "the ID is still there")
        #expect(id.current == "AKID" && secret.current == nil, "the other key is still removed")
        let s = AppSettings(defaults: UserDefaults(suiteName: testSuiteName())!)
        let m = model(s, keys: k)
        m.removeKeys()
        #expect(m.objectFailed && m.objectMessage == "The keys couldn't be removed from your Keychain.")
        #expect(m.accessKeyIDDraft == "AKID", "still shown: still saved")
        id.refuse = false
        m.removeKeys()
        #expect(!m.objectFailed && m.objectMessage == "The keys were removed from your Keychain.")
    }

    @Test func keychainStatusesAreReadCarefully() {
        #expect(KeychainItem.lookup(status: errSecSuccess, data: Data("k".utf8)) == .found("k"))
        #expect(KeychainItem.lookup(status: errSecSuccess, data: nil) == .failed)
        #expect(KeychainItem.lookup(status: errSecItemNotFound, data: nil) == .notFound)
        #expect(KeychainItem.lookup(status: errSecInteractionNotAllowed, data: nil) == .failed)
        #expect(KeychainItem.lookup(status: errSecAuthFailed, data: nil) == .failed)
        #expect(KeychainItem.removed(errSecSuccess) && KeychainItem.removed(errSecItemNotFound))
        #expect(!KeychainItem.removed(errSecInteractionNotAllowed) && !KeychainItem.removed(errSecAuthFailed))
        // An item without `lookup` reads nil as "no such item".
        let plain = KeychainItem(read: { nil }, write: { _ in true })
        #expect(plain.lookup() == .notFound && KeychainLookup.found("x").value == "x")
    }

    /// No test may read or replace the user's real keys: in a test run every
    /// `KeychainItem.keychain` item (Immich, archive, Google Photos) lives in memory.
    @Test func testsNeverTouchTheRealKeychain() {
        #expect(KeychainItem.inTestProcess)
        let a = KeychainItem.keychain(service: "app.luxtable.Luxtable.test", account: "probe")
        let b = KeychainItem.keychain(service: "app.luxtable.Luxtable.test", account: "probe")
        #expect(a.write("x") && b.read() == "x", "shared like the real Keychain")
        #expect(a.write(nil) && b.lookup() == .notFound)
    }

    @Test func settingsAreValidatedByTheArchiveRules() {
        #expect(store().problem == nil)
        #expect(store(bucket: "").problem?.contains("bucket") == true)
        #expect(store(prefix: "a/../b").problem?.contains("..") == true)
        #expect(store(endpoint: "ftp://x").problem != nil)
        #expect(store(endpoint: "http://s3.example.com").problem?.contains("use https://") == true)
        #expect(store(endpoint: "http://192.168.1.20:9000", pathStyle: true).problem == nil, "a LAN MinIO")
        #expect(store(bucket: "my.photos").problem?.contains("path-style") == true)
        #expect(store(bucket: "my.photos", pathStyle: true).problem == nil)
        #expect(store(prefix: "/a//b").layout == "s3://photos/a/b/")
        #expect(store(prefix: "").layout == "s3://photos/")
    }

    private func model(_ s: AppSettings, keys k: ArchiveObjectKeys,
                       tester: @escaping ArchiveSettingsModel.ObjectTester = { _, _ in
                           ObjectConnectionTest(testKey: "luxtable/.lt-archive-test/connection-test.txt",
                                                location: "s3://photos/luxtable/")
                       }) -> ArchiveSettingsModel {
        ArchiveSettingsModel(settings: s, checker: { _ in .notSet }, exists: { _ in false }, appInstalled: { false },
                             objectKeys: k, objectTester: tester)
    }

    @Test func savingObjectStorageValidatesAndKeepsTheSecretOutOfPreferences() {
        let s = AppSettings(defaults: UserDefaults(suiteName: testSuiteName())!)
        let (k, _, secret) = keys()
        let m = model(s, keys: k)
        m.destination = .object
        #expect(s.archiveDestination == .object)
        m.objectDraft = store(bucket: "")
        m.saveObjectStore()
        #expect(m.objectFailed && m.objectMessage?.hasPrefix("Enter a bucket") == true)
        #expect(m.objectLayoutText == "Enter the bucket and its path.")
        m.objectDraft = store()
        #expect(m.objectProblem == nil)
        #expect(m.objectLayoutText == "Sessions go to s3://photos/luxtable/<session name>/…")
        m.saveObjectStore()
        #expect(m.objectFailed && m.objectMessage == "Enter the access key ID and the secret access key.")
        m.accessKeyIDDraft = "AKID"
        m.secretDraft = "s3cr3t"
        m.saveObjectStore()
        #expect(!m.objectFailed && m.objectMessage == "Saved. The keys are in your Keychain.")
        #expect(s.archiveObjectStore == store() && secret.current == "s3cr3t" && m.secretDraft.isEmpty)
        #expect(m.hasSavedSecret)
        let all = s.userDefaults.dictionaryRepresentation().values.map { "\($0)" }
        #expect(!all.contains { $0.contains("s3cr3t") || $0.contains("AKID") }, "keys never in preferences")
        // A refusing Keychain is reported.
        secret.refuse = true
        m.secretDraft = "new"
        m.saveObjectStore()
        #expect(m.objectFailed && m.objectMessage == "The keys couldn't be saved in the Keychain.")
        secret.refuse = false
        m.removeKeys()
        #expect(!m.hasSavedSecret && m.accessKeyIDDraft.isEmpty && !m.objectFailed)
        // Back to a folder: the object settings stay for later, nothing is moved.
        m.destination = .folder
        #expect(s.archiveSnapshot.objectStore == nil && s.archiveObjectStore == store())
        #expect(ArchiveSettingsModel.switchNote.contains("never moves or deletes"))
        #expect(ArchiveSettingsModel.objectNote.contains("Nothing is ever deleted"))
    }

    @Test func testConnectionReportsInWords() async {
        let s = AppSettings(defaults: UserDefaults(suiteName: testSuiteName())!)
        let (k, _, _) = keys("AKID", "saved-secret")
        let seen = Seen()
        let ok = model(s, keys: k, tester: { settings, creds in
            seen.add("\(settings.bucket)/\(creds.accessKeyId)/\(creds.secretAccessKey)")
            return ObjectConnectionTest(testKey: "luxtable/.lt-archive-test/connection-test.txt", location: "s3://photos/luxtable/")
        })
        #expect(ok.accessKeyIDDraft == "AKID", "the saved ID is shown")
        ok.objectDraft = store(bucket: "x")
        await ok.testConnection()
        #expect(ok.objectFailed && seen.all.isEmpty, "invalid settings never reach the service")
        ok.objectDraft = store()
        await ok.testConnection()
        #expect(!ok.objectFailed && !ok.testingConnection)
        #expect(ok.objectMessage == "Connected. A small test object was left at luxtable/.lt-archive-test/connection-test.txt; "
            + "the next test overwrites it.")
        #expect(seen.all == ["photos/AKID/saved-secret"], "tested with the saved secret")
        ok.secretDraft = "typed"
        await ok.testConnection()
        #expect(seen.all.last == "photos/AKID/typed", "tested as typed, before saving")
        ok.accessKeyIDDraft = ""
        await ok.testConnection()
        #expect(ok.objectFailed && ok.objectMessage == "Enter the access key ID and the secret access key.")

        for (error, words) in [
            (DsError.InvalidArgument(message: "the storage service refused the access key: InvalidAccessKeyId"),
             "The storage service refused the access key: InvalidAccessKeyId"),
            (DsError.Io(message: "the storage service didn't answer: offline"), "The storage service didn't answer: offline"),
            (DsError.Cancelled, "The test was cancelled."),
        ] {
            let failing = model(s, keys: k, tester: { _, _ in throw error })
            failing.objectDraft = store()
            await failing.testConnection()
            #expect(failing.objectFailed && failing.objectMessage == words)
        }
        let other = model(s, keys: k, tester: { _, _ in throw CocoaError(.featureUnsupported) })
        other.objectDraft = store()
        await other.testConnection()
        #expect(other.objectFailed && other.objectMessage?.hasPrefix("The test didn't finish") == true)
        #expect(ArchiveSettingsModel.words(.Unsupported(message: "x")).contains("x"))
    }

    @Test func folderKindIsShownWhenReady() async {
        let s = AppSettings(defaults: UserDefaults(suiteName: testSuiteName())!)
        let (k, _, _) = keys()
        s.archiveTarget = "/Volumes/NAS/Photos"
        let vol = ArchiveSettingsModel(settings: s, checker: { _ in .ready(fsType: "apfs") }, exists: { _ in true },
                                       appInstalled: { false }, objectKeys: k)
        #expect(vol.folderKindText == nil, "not checked yet")
        await vol.checkTarget()
        #expect(vol.folderKindText?.hasPrefix("Mounted volume") == true)
        s.archiveTarget = "/Users/me/Archive"
        await vol.checkTarget()
        #expect(vol.folderKindText == "Local folder on this Mac.")
        let share = ArchiveSettingsModel(settings: s, checker: { _ in .ready(fsType: "smbfs") }, exists: { _ in true },
                                         appInstalled: { false }, objectKeys: k)
        await share.checkTarget()
        #expect(share.folderKindText?.hasPrefix("Mounted volume") == true)
    }

    @Test func immichSkipsAlbumSyncWithObjectStorage() {
        let r = ImmichSyncReport(finishedMs: 0, sessions: 0, albums: [], missing: 0, scannedLibraries: 0,
                                 scanNote: "The archive goes to object storage, which Immich can't index: album sync is skipped.",
                                 failures: [], error: nil, offline: false, keyRefused: false)
        #expect(ImmichModel.summary(r).contains("object storage"))
        #expect(ImmichModel.objectStorageNote.contains("Settings ▸ Archive"))
    }

    @Test func theServiceHandsTheSettingsAndKeysToTheArchive() async throws {
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: tmp) }
        try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        // A bucket on its own prefix (the archive lock is per destination).
        let object = store(prefix: "swift-\(UUID().uuidString)/")
        let snap = ArchiveSettingsSnapshot(enabled: true, target: "", intervalMinutes: 10, amazonFolder: nil,
                                           objectStore: object)
        #expect(RustArchiveService.config(snap, lockDir: nil, debounce: 60).objectStore == object)
        let (k, _, _) = keys()
        let http = CannedHTTP()
        let service = RustArchiveService(initial: snap, settings: { snap }, handles: { _ in nil },
                                         manifest: tmp.appendingPathComponent("manifest.json"), lockDir: tmp.path,
                                         tick: .milliseconds(50), watchVolumes: false, http: http, objectKeys: k)
        // No keys: the archive says so (and sends nothing).
        let deadline = ContinuousClock.now + TestTiming.bound(.seconds(10))
        func reason() async -> String? {
            await service.refresh()
            return service.status.detail?.targetReason
        }
        while await reason()?.contains("access key isn't set") != true {
            #expect(ContinuousClock.now < deadline, "no keys reported")
            if ContinuousClock.now >= deadline { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        #expect(http.count == 0)
        // Keys saved: the next refresh hands them over; the bucket answers 403.
        k.save(accessKeyID: "AKID", secret: "s3cr3t")
        while await reason()?.contains("refused the access key") != true {
            #expect(ContinuousClock.now < deadline, "keys handed over")
            if ContinuousClock.now >= deadline { return }
            try await Task.sleep(for: .milliseconds(50))
        }
        #expect(http.count > 0 && http.lastAuthorization?.contains("Credential=AKID/") == true)
        #expect(http.lastAuthorization?.contains("s3cr3t") == false)
        await service.stop()
        // A settings change of the object store alone is applied.
        var moved = snap
        moved.objectStore = store(bucket: "other")
        let r = RustArchiveService.applyAll(from: snap, to: moved, setConfig: { _ in }, setAmazon: { _ in })
        #expect(r.applied.objectStore == moved.objectStore && r.error == nil)
    }

    @Test func urlSessionTransportRefusesOtherSchemes() {
        #expect(throws: DsError.self) {
            _ = try URLSessionArchiveHTTP().send(request: ArchiveHttpRequest(method: "HEAD", url: "ftp://bucket.example/x",
                                                                             headers: [], body: Data()))
        }
    }
}

private final class Seen: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [String] = []
    func add(_ s: String) { lock.withLock { items.append(s) } }
    var all: [String] { lock.withLock { items } }
}

/// Answers every request 403 (no body, like `HeadBucket`), recording what was signed.
private final class CannedHTTP: ArchiveHttp, @unchecked Sendable {
    private let lock = NSLock()
    private var sent: [ArchiveHttpRequest] = []
    func send(request: ArchiveHttpRequest) throws -> ArchiveHttpResponse {
        lock.withLock { sent.append(request) }
        return ArchiveHttpResponse(status: 403, headers: [], body: Data())
    }
    var count: Int { lock.withLock { sent.count } }
    var lastAuthorization: String? {
        lock.withLock { sent.last?.headers.first { $0.name.lowercased() == "authorization" }?.value }
    }
}

private final class ArchiveStub: URLProtocol {
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

@Suite(.serialized) struct URLSessionArchiveHTTPTests {
    private func http() -> URLSessionArchiveHTTP {
        let c = URLSessionConfiguration.ephemeral
        c.protocolClasses = [ArchiveStub.self]
        return URLSessionArchiveHTTP(session: URLSession(configuration: c))
    }

    @Test func returnsStatusHeadersAndBody() throws {
        ArchiveStub.seen = []
        ArchiveStub.answer = { _ in (200, ["ETag": "\"abc\"", "x-amz-meta-lt-blake3": "h"], Data("ok".utf8)) }
        let r = try http().send(request: ArchiveHttpRequest(
            method: "PUT", url: "https://photos.s3.example.test/luxtable/a%20b.ARW",
            headers: [ArchiveHeader(name: "Content-MD5", value: "md5")], body: Data("body".utf8)))
        #expect(r.status == 200 && r.body == Data("ok".utf8))
        #expect(r.headers.contains { $0.name.lowercased() == "etag" && $0.value == "\"abc\"" })
        #expect(r.headers.contains { $0.name.lowercased() == "x-amz-meta-lt-blake3" && $0.value == "h" })
        let sent = try #require(ArchiveStub.seen.last)
        #expect(sent.httpMethod == "PUT" && sent.value(forHTTPHeaderField: "Content-MD5") == "md5")
        #expect(sent.url?.absoluteString == "https://photos.s3.example.test/luxtable/a%20b.ARW", "the signed path is kept")
        ArchiveStub.answer = { _ in (404, [:], Data()) }
        let head = try http().send(request: ArchiveHttpRequest(method: "HEAD", url: "https://photos.s3.example.test/x",
                                                               headers: [], body: Data()))
        #expect(head.status == 404)
    }

    /// https anywhere; plain http only to a server on this network (a LAN MinIO).
    @Test func plainHTTPOnlyOnThisNetwork() throws {
        ArchiveStub.seen = []
        ArchiveStub.answer = { _ in (200, [:], Data()) }
        for url in ["http://s3.example.com/b", "http://8.8.8.8:9000/b", "http://172.32.0.1/b", "http://10.1/b",
                    "http://nas.local.example.com/b", "http://[2001:db8::1]/b"] {
            #expect(throws: DsError.self, "\(url)") {
                _ = try http().send(request: ArchiveHttpRequest(method: "HEAD", url: url, headers: [], body: Data()))
            }
        }
        #expect(ArchiveStub.seen.isEmpty, "nothing went out")
        for url in ["http://127.0.0.1:9000/b", "http://localhost:9000/b", "http://10.0.0.5/b", "http://172.20.1.1/b",
                    "http://192.168.1.20:9000/b", "http://169.254.1.1/b", "http://nas.local:9000/b", "http://nas/b",
                    "http://[::1]:9000/b", "https://s3.example.com/b"] {
            #expect(try http().send(request: ArchiveHttpRequest(method: "HEAD", url: url, headers: [], body: Data())).status
                == 200, "\(url)")
        }
        #expect(BlockingHTTP.allowsPlainHTTP(host: "minio.localhost") && !BlockingHTTP.allowsPlainHTTP(host: ""))
        // Each caller's policy: Immich may use http anywhere, Google Photos never.
        let open = try #require(URL(string: "http://immich.example.org/api"))
        let s = BlockingHTTP.makeSession()
        #expect(BlockingHTTP(session: s, schemes: .httpsOrHTTP, timeout: 1).problem(open, raw: "") == nil)
        #expect(BlockingHTTP(session: s, schemes: .httpsOnly, timeout: 1).problem(open, raw: "") != nil)
        #expect(BlockingHTTP(session: s, schemes: .httpsOrLocalHTTP, timeout: 1).problem(open, raw: "")?
            .contains("use https:// for immich.example.org") == true)
        #expect(BlockingHTTP(session: s, schemes: .httpsOrHTTP, timeout: 1)
            .problem(try #require(URL(string: "file:///etc/hosts")), raw: "f") == "bad URL f")
        #expect(!BlockingHTTP.allowsPlainHTTP(host: "300.1.1.1") && !BlockingHTTP.allowsPlainHTTP(host: "fe80::1"))
    }

    /// A body-less PUT (CopyObject, a zero-byte object) still carries a body, so it goes
    /// out with `Content-Length: 0`; GET and HEAD carry none. No cookies either way.
    @Test func requestsAlwaysCarryABodyAndNoCookies() throws {
        let h = BlockingHTTP(session: BlockingHTTP.makeSession(), schemes: .httpsOnly, timeout: 5)
        let url = try #require(URL(string: "https://photos.s3.example.test/k"))
        for method in ["PUT", "POST", "DELETE"] {
            let r = h.request(method: method, url: url, headers: [("x-amz-copy-source", "/photos/a")], body: Data())
            #expect(r.httpBody == Data(), "\(method)")
            #expect(!r.httpShouldHandleCookies && r.value(forHTTPHeaderField: "x-amz-copy-source") == "/photos/a")
        }
        #expect(h.request(method: "PUT", url: url, headers: [], body: Data("b".utf8)).httpBody == Data("b".utf8))
        #expect(h.request(method: "GET", url: url, headers: [], body: Data()).httpBody == nil)
        #expect(h.request(method: "HEAD", url: url, headers: [], body: Data()).httpBody == nil)
        let c = BlockingHTTP.makeSession().configuration
        #expect(c.httpCookieStorage == nil && !c.httpShouldSetCookies && c.httpCookieAcceptPolicy == .never)
        #expect(c.urlCache == nil)
        // Through the transport: an empty PUT is sent.
        ArchiveStub.seen = []
        ArchiveStub.answer = { _ in (200, [:], Data()) }
        _ = try http().send(request: ArchiveHttpRequest(method: "PUT", url: "https://photos.s3.example.test/empty",
                                                       headers: [], body: Data()))
        #expect(ArchiveStub.seen.last?.httpMethod == "PUT")
    }

    @Test func noAnswerThrows() {
        ArchiveStub.answer = { _ in nil }
        #expect(throws: DsError.self) {
            _ = try http().send(request: ArchiveHttpRequest(method: "HEAD", url: "https://photos.s3.example.test/",
                                                           headers: [], body: Data()))
        }
    }
}
