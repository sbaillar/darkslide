import CoreGraphics
import Foundation
import Observation

/// Settings ▸ Devices and the link server's life: off by default, on only while
/// the app runs and the user allows it; pairing, the paired devices, write
/// access, and the transfer indicator.
@MainActor @Observable
public final class RemoteLinkModel {
    static let enabledKey = "allowRemoteDevices"

    /// "Allow iPad connections" (persisted; off until the user turns it on).
    public private(set) var enabled: Bool
    public private(set) var port: UInt16?
    /// This Mac's key fingerprint, short form (shown next to the QR code).
    public private(set) var fingerprint: String?
    public private(set) var devices: [LinkedDevice] = []
    public private(set) var pairing: LinkPairing?
    public private(set) var status = LinkStatus()
    public private(set) var error: String?

    @ObservationIgnored let backend: any LinkBackend
    @ObservationIgnored let advertiser: any LinkAdvertiser
    @ObservationIgnored let stateDir: URL
    @ObservationIgnored let defaults: UserDefaults
    /// The Mac's names, looked up when first needed (a hostname lookup can stall on a bad network).
    @ObservationIgnored private var macNameOverride: String?
    @ObservationIgnored private var hostOverride: String?
    @ObservationIgnored var macName: String {
        if macNameOverride == nil { macNameOverride = PlatformWorkspace.computerName ?? "Mac" }
        return macNameOverride!
    }
    @ObservationIgnored var host: String {
        if hostOverride == nil { hostOverride = ProcessInfo.processInfo.hostName }
        return hostOverride!
    }
    @ObservationIgnored private var server: (any LinkServer)?
    /// What the server offers (the open session).
    @ObservationIgnored public private(set) var served: LinkSession?
    @ObservationIgnored private var poller: Task<Void, Never>?
    @ObservationIgnored private var pairingDeadline: Date?
    /// Session roots a device changed (the app reloads the open one).
    @ObservationIgnored public var onSessionsChanged: @MainActor ([String]) -> Void = { _ in }

    public init(backend: any LinkBackend = RustLinkBackend(), advertiser: any LinkAdvertiser = DNSSDAdvertiser(),
                stateDir: URL = AppPaths.support.appendingPathComponent("Remote", isDirectory: true),
                defaults: UserDefaults, macName: String? = nil, host: String? = nil) {
        self.backend = backend
        self.advertiser = advertiser
        self.stateDir = stateDir
        self.defaults = defaults
        macNameOverride = macName
        hostOverride = host
        enabled = defaults.bool(forKey: Self.enabledKey)
    }

    public var running: Bool { server != nil }

    /// At launch: serve again if the user left it on; list the paired devices.
    public func resume() {
        reloadDevices()
        if enabled { start() }
    }

    public func setEnabled(_ on: Bool) {
        enabled = on
        defaults.set(on, forKey: Self.enabledKey)
        if on { start() } else { stop() }
    }

    private func start() {
        guard server == nil else { return }
        do {
            let s = try backend.start(stateDir: stateDir, name: macName, port: backend.defaultPort)
            s.serve(served)
            server = s
            port = s.port
            fingerprint = s.fingerprint
            error = nil
            if !advertiser.advertise(name: macName, port: s.port) {
                error = "Bonjour isn't advertising this Mac; pair with the QR code."
            }
            poller = Task { [weak self] in
                while !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(1))
                    guard let self else { return }
                    self.tick()
                }
            }
        } catch {
            self.error = "Couldn't start: \(error.localizedDescription)"
        }
    }

    /// Stop serving (the toggle, or the app quitting).
    public func stop() {
        poller?.cancel()
        poller = nil
        advertiser.stop()
        server?.stop()
        server = nil
        port = nil
        pairing = nil
        pairingDeadline = nil
        status = LinkStatus()
    }

    /// The session a device may see (the app passes its open one).
    public func serve(_ session: LinkSession?) {
        served = session
        server?.serve(session)
    }

    public func beginPairing(now: Date = Date()) {
        guard let server else { return }
        let p = server.beginPairing(host: host)
        pairing = p
        pairingDeadline = now.addingTimeInterval(p.expiresIn)
    }

    public func cancelPairing() {
        server?.cancelPairing()
        pairing = nil
        pairingDeadline = nil
    }

    /// The pairing QR code.
    /// Made once per pairing: the view reads it again every second as the status ticks.
    public var qrImage: CGImage? {
        guard let payload = pairing?.qrPayload else { return nil }
        if qrCache?.payload != payload { qrCache = (payload, QRCode.image(for: payload)) }
        return qrCache?.image
    }
    @ObservationIgnored private var qrCache: (payload: String, image: CGImage?)?

    public func reloadDevices() {
        do {
            devices = try backend.devices(stateDir: stateDir).sorted { $0.pairedAt < $1.pairedAt }
        } catch {
            self.error = "The paired-device list can't be read: \(error.localizedDescription)"
        }
    }

    /// Forget a device: its next request is refused.
    public func revoke(_ device: LinkedDevice) {
        run { try self.backend.revoke(stateDir: self.stateDir, fingerprint: device.fingerprint) }
    }

    public func setWrite(_ device: LinkedDevice, _ write: Bool) {
        run { try self.backend.setWrite(stateDir: self.stateDir, fingerprint: device.fingerprint, write: write) }
    }

    private func run(_ change: () throws -> Void) {
        do {
            try change()
            error = nil
            reloadDevices()
        } catch {
            reloadDevices()
            self.error = error.localizedDescription // the change's failure, not the reload's
        }
    }

    /// One poll (every second while serving): status, sessions a device
    /// changed, and whether pairing finished or expired.
    public func tick(now: Date = Date()) {
        guard let server else { return }
        status = server.status()
        let changed = server.takeChangedSessions()
        if !changed.isEmpty { onSessionsChanged(changed) }
        guard pairing != nil else { return }
        let before = devices.count
        reloadDevices()
        if devices.count > before || pairingDeadline.map({ now >= $0 }) ?? false {
            pairing = nil // a code works once; the server closed the window
            pairingDeadline = nil
        }
    }

    /// The toolbar indicator: nil when nothing is connected.
    public var indicator: (symbol: String, label: String)? {
        guard running, status.connections > 0 else { return nil }
        if status.transferring { return ("arrow.up.arrow.down.circle.fill", "Transferring with iPad") }
        return ("ipad.landscape", status.connections == 1 ? "iPad connected" : "\(status.connections) devices connected")
    }
}
