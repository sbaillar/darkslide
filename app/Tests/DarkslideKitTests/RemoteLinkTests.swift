import Foundation
import Testing
@testable import DarkslideKit

/// [remote] Settings ▸ Devices and the link server's life, on fakes; plus the
/// live FFI backend on loopback and the DNS-SD advertiser on this Mac only.
@MainActor @Suite struct RemoteLinkTests {
    final class FakeServer: LinkServer, @unchecked Sendable {
        var session: LinkSession?
        var stopped = false
        var cancelled = false
        var nextStatus = LinkStatus()
        var changed: [String] = []
        let port: UInt16 = 4242
        let fingerprint = "ab12 cd34 ef56 7890"

        func serve(_ session: LinkSession?) { self.session = session }
        func beginPairing(host: String) -> LinkPairing {
            LinkPairing(code: "123456", qrPayload: "darkslide-pair://\(host):4242?fp=00&t=11", expiresIn: 300)
        }
        func cancelPairing() { cancelled = true }
        func status() -> LinkStatus { nextStatus }
        func takeChangedSessions() -> [String] {
            defer { changed = [] }
            return changed
        }
        func stop() { stopped = true }
    }

    final class FakeBackend: LinkBackend, @unchecked Sendable {
        var server = FakeServer()
        var startError: Error?
        var listError: Error?
        var devicesList: [LinkedDevice] = []
        var starts = 0
        let defaultPort: UInt16 = 47_474

        func start(stateDir: URL, name: String, port: UInt16) throws -> any LinkServer {
            if let startError { throw startError }
            starts += 1
            server = FakeServer()
            return server
        }
        func devices(stateDir: URL) throws -> [LinkedDevice] {
            if let listError { throw listError }
            return devicesList
        }
        func revoke(stateDir: URL, fingerprint: String) throws {
            if let listError { throw listError }
            devicesList.removeAll { $0.fingerprint == fingerprint }
        }
        func setWrite(stateDir: URL, fingerprint: String, write: Bool) throws {
            if let i = devicesList.firstIndex(where: { $0.fingerprint == fingerprint }) { devicesList[i].write = write }
        }
    }

    final class FakeAdvertiser: LinkAdvertiser {
        var advertised: (String, UInt16)?
        var works = true
        func advertise(name: String, port: UInt16) -> Bool {
            advertised = works ? (name, port) : nil
            return works
        }
        func stop() { advertised = nil }
    }

    struct Boom: LocalizedError { var errorDescription: String? { "boom" } }

    static func device(_ n: Int, write: Bool = false) -> LinkedDevice {
        LinkedDevice(fingerprint: String(repeating: "\(n)", count: 64), shortFingerprint: "\(n)", name: "iPad \(n)",
                     pairedAt: Date(timeIntervalSince1970: Double(n)), write: write)
    }

    func makeLink(_ backend: FakeBackend, _ adv: FakeAdvertiser = FakeAdvertiser(),
                  defaults: UserDefaults? = nil) -> RemoteLinkModel {
        RemoteLinkModel(backend: backend, advertiser: adv, stateDir: URL(fileURLWithPath: "/nonexistent/remote"),
                        defaults: defaults ?? makeSettings().userDefaults, macName: "Studio Mac", host: "mac.local")
    }

    @Test func offByDefaultAndTheToggleStartsStopsAndPersists() {
        let backend = FakeBackend()
        let adv = FakeAdvertiser()
        let defaults = makeSettings().userDefaults
        let link = makeLink(backend, adv, defaults: defaults)
        #expect(!link.enabled && !link.running)
        link.resume()
        #expect(backend.starts == 0, "nothing binds until the user turns it on")
        link.serve(LinkSession(root: URL(fileURLWithPath: "/s/Trip"), name: "Trip", handle: NSObject()))
        link.setEnabled(true)
        #expect(link.running && link.port == 4242 && link.fingerprint == "ab12 cd34 ef56 7890")
        #expect(backend.server.session?.root.path == "/s/Trip", "the open session is served")
        #expect(adv.advertised?.0 == "Studio Mac" && adv.advertised?.1 == 4242)
        link.setEnabled(true)
        #expect(backend.starts == 1, "starting twice is a no-op")
        link.serve(nil)
        #expect(backend.server.session == nil)
        #expect(link.served == nil)
        let server = backend.server
        link.setEnabled(false)
        #expect(server.stopped && !link.running && link.port == nil && adv.advertised == nil)

        // Remembered across launches: on again at the next start.
        defaults.set(true, forKey: RemoteLinkModel.enabledKey)
        let again = makeLink(backend, adv, defaults: defaults)
        #expect(again.enabled)
        again.resume()
        #expect(again.running && backend.starts == 2)
        again.stop()
    }

    @Test func startAndBonjourFailuresAreShown() {
        let backend = FakeBackend()
        backend.startError = Boom()
        let link = makeLink(backend)
        link.setEnabled(true)
        #expect(!link.running && link.error?.contains("boom") == true)
        backend.startError = nil
        let adv = FakeAdvertiser()
        adv.works = false
        let l2 = makeLink(backend, adv)
        l2.setEnabled(true)
        #expect(l2.running && l2.error?.contains("Bonjour") == true)
        l2.stop()
    }

    @Test func pairingShowsACodeAndClosesWhenADeviceJoinsOrItExpires() {
        let backend = FakeBackend()
        let link = makeLink(backend)
        link.beginPairing()
        #expect(link.pairing == nil, "not while the server is off")
        link.setEnabled(true)
        let t0 = Date(timeIntervalSince1970: 1000)
        link.beginPairing(now: t0)
        #expect(link.pairing?.code == "123456")
        #expect(link.pairing?.qrPayload.contains("mac.local:4242") == true)
        #expect(link.qrImage != nil)
        link.tick(now: t0.addingTimeInterval(10))
        #expect(link.pairing != nil)
        backend.devicesList = [Self.device(1)]
        link.tick(now: t0.addingTimeInterval(20))
        #expect(link.pairing == nil && link.devices.count == 1, "paired: the code is spent")

        link.beginPairing(now: t0)
        link.tick(now: t0.addingTimeInterval(301))
        #expect(link.pairing == nil, "expired")
        link.beginPairing(now: t0)
        link.cancelPairing()
        #expect(link.pairing == nil && backend.server.cancelled)
        #expect(link.qrImage == nil)
        link.stop()
    }

    @Test func devicesAreListedGrantedAndRevoked() {
        let backend = FakeBackend()
        backend.devicesList = [Self.device(2), Self.device(1)]
        let link = makeLink(backend)
        link.reloadDevices()
        #expect(link.devices.map(\.name) == ["iPad 1", "iPad 2"], "oldest first")
        link.setWrite(link.devices[0], true)
        #expect(link.devices[0].write)
        link.revoke(link.devices[1])
        #expect(link.devices.map(\.name) == ["iPad 1"])
        backend.listError = Boom()
        link.revoke(link.devices[0])
        #expect(link.error == "boom")
        link.reloadDevices()
        #expect(link.error?.contains("can't be read") == true)
    }

    @Test func ticksReportStatusChangesAndTheIndicator() {
        let backend = FakeBackend()
        let link = makeLink(backend)
        var changed: [[String]] = []
        link.onSessionsChanged = { changed.append($0) }
        link.tick()
        #expect(link.indicator == nil, "not running")
        link.setEnabled(true)
        link.tick()
        #expect(link.indicator == nil, "nobody connected")
        backend.server.nextStatus = LinkStatus(connections: 1)
        link.tick()
        #expect(link.indicator?.label == "iPad connected")
        backend.server.nextStatus = LinkStatus(connections: 2, transferring: true, bytesSent: 10)
        backend.server.changed = ["/s/Trip"]
        link.tick()
        #expect(link.indicator?.label == "Transferring with iPad")
        #expect(changed == [["/s/Trip"]])
        link.tick()
        #expect(changed.count == 1, "only new changes are reported")
        backend.server.nextStatus = LinkStatus(connections: 2)
        link.tick()
        #expect(link.indicator?.label == "2 devices connected")
        link.stop()
        #expect(link.status == LinkStatus())
    }

    @Test func theAppServesTheOpenSessionAndReloadsAfterAPush() async throws {
        let inner = MockSessionStore(counts: [3])
        let store = RootedStore(inner: inner)
        let model = makeModel(store: store)
        await model.start()
        #expect(await waitUntil { model.currentSession != nil })
        let root = try #require(await store.linkSession(for: model.currentSession!)?.root)
        #expect(await waitUntil { model.remote.served?.root == root })
        await model.linkChanged(["/elsewhere"])
        #expect(model.toast == nil)
        // A save queued while the reload runs goes out before the device edits
        // count as adopted (so the store still keeps a device edit it replaces).
        #expect(await store.events.last == "seen", "an open adopts the stored edits")
        model.setRating(4)
        await model.linkChanged([root.path])
        #expect(model.toast?.message == "Updated from your iPad")
        let events = await store.events
        let lastSave = try #require(events.lastIndex(of: "save"))
        #expect(events[lastSave...].contains("seen") && events.last == "seen", "\(events)")

        // A store that can't say where a session lives serves nothing.
        let plain = makeModel()
        await plain.start()
        #expect(await waitUntil { plain.currentSession != nil })
        await plain.serveCurrentSessionOnLink()
        #expect(plain.remote.served == nil)
        await plain.linkChanged(["/x"])
        #expect(plain.toast == nil)
    }

    /// The real `ds-remote` server through the FFI, on loopback.
    @Test func liveBackendStartsListsAndStops() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("remote-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let backend = RustLinkBackend(allInterfaces: false)
        #expect(backend.defaultPort == 47_474)
        let server = try backend.start(stateDir: dir, name: "Test Mac", port: 0)
        #expect(server.port > 0 && server.fingerprint.count == 19)
        server.serve(LinkSession(root: dir, name: "x", handle: NSObject())) // not a SessionHandle: serves nothing
        server.serve(nil)
        let p = server.beginPairing(host: "localhost")
        #expect(p.code.count == 6 && p.expiresIn == 300 && p.qrPayload.contains("localhost:\(server.port)"))
        server.cancelPairing()
        #expect(server.status() == LinkStatus())
        #expect(server.takeChangedSessions().isEmpty)
        #expect(try backend.devices(stateDir: dir).isEmpty)
        try backend.revoke(stateDir: dir, fingerprint: "00")
        try backend.setWrite(stateDir: dir, fingerprint: "00", write: true)
        server.stop()
        try Data("{".utf8).write(to: dir.appendingPathComponent("devices.json"))
        #expect(throws: (any Error).self) { try backend.devices(stateDir: dir) }
    }

    /// Registered on this Mac only (never advertised on the network from a test).
    @Test func dnssdAdvertiserRegistersAndReleases() {
        let a = DNSSDAdvertiser(interface: DNSSDAdvertiser.localOnly)
        #expect(!a.isAdvertising)
        #expect(a.advertise(name: "Darkslide test", port: 4242))
        #expect(a.isAdvertising)
        #expect(a.advertise(name: "Darkslide test 2", port: 4243), "replaces the first")
        a.stop()
        #expect(!a.isAdvertising)
        #expect(DNSSDAdvertiser.serviceType == "_darkslide._tcp")
    }

    @Test func qrCodesRender() throws {
        let img = try #require(QRCode.image(for: "darkslide-pair://mac.local:47474?fp=00&t=11", scale: 4))
        #expect(img.width == img.height && img.width >= 21 * 4)
    }
}

/// A mock store that knows its sessions' roots (as `RustSessionStore` does).
actor RootedStore: LinkSessionProviding {
    let inner: MockSessionStore
    /// What the app asked of the store, in order (saves and "seen").
    private(set) var events: [String] = []
    init(inner: MockSessionStore) { self.inner = inner }
    func markDeviceEditsSeen(in session: SessionInfo) async { events.append("seen") }
    func linkSession(for session: SessionInfo) async -> LinkSession? {
        LinkSession(root: URL(fileURLWithPath: "/sessions/\(session.name)"), name: session.name, handle: NSObject())
    }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws {
        events.append("save")
        try await inner.save(photos: photos)
    }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws {
        events.append("save")
        try await inner.save(edits: edits)
    }
}
