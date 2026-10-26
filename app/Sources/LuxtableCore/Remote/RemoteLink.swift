import LuxtableFFI
import Foundation

/// A paired iPad (or test client), as Settings ▸ Devices lists it.
public struct LinkedDevice: Identifiable, Equatable, Sendable {
    public var id: String { fingerprint }
    public let fingerprint: String
    /// `ab12 cd34 ef56 7890`, to compare with what the iPad shows.
    public let shortFingerprint: String
    public let name: String
    public let pairedAt: Date
    /// May push photos and edits; read-only until granted.
    public var write: Bool
}

/// An open pairing window: the code to type and the QR code's text.
public struct LinkPairing: Equatable, Sendable {
    public let code: String
    public let qrPayload: String
    public let expiresIn: TimeInterval
}

public struct LinkStatus: Equatable, Sendable {
    public var connections: Int = 0
    /// Data moved in the last two seconds.
    public var transferring = false
    public var bytesSent: UInt64 = 0
    public var bytesReceived: UInt64 = 0

    public init(connections: Int = 0, transferring: Bool = false, bytesSent: UInt64 = 0, bytesReceived: UInt64 = 0) {
        self.connections = connections
        self.transferring = transferring
        self.bytesSent = bytesSent
        self.bytesReceived = bytesReceived
    }
}

/// The session the link serves: the app's open one, through its own handle,
/// so a device's writes and the app's share one lock.
public struct LinkSession: @unchecked Sendable { // the handle is thread-safe (Rust)
    public let root: URL
    public let name: String
    /// The store's `SessionHandle` (opaque here; a test double in tests).
    public let handle: AnyObject

    public init(root: URL, name: String, handle: AnyObject) {
        self.root = root
        self.name = name
        self.handle = handle
    }
}

/// A running link server (`lt-remote`).
public protocol LinkServer: AnyObject, Sendable {
    var port: UInt16 { get }
    /// This Mac's key fingerprint, short form.
    var fingerprint: String { get }
    /// Serve `session` (`nil`: nothing).
    func serve(_ session: LinkSession?)
    func beginPairing(host: String) -> LinkPairing
    func cancelPairing()
    func status() -> LinkStatus
    /// Session roots a device changed since the last call.
    func takeChangedSessions() -> [String]
    func stop()
}

/// The link server and paired-device list; the seam over the FFI.
public protocol LinkBackend: Sendable {
    func start(stateDir: URL, name: String, port: UInt16) throws -> any LinkServer
    func devices(stateDir: URL) throws -> [LinkedDevice]
    func revoke(stateDir: URL, fingerprint: String) throws
    func setWrite(stateDir: URL, fingerprint: String, write: Bool) throws
    var defaultPort: UInt16 { get }
}

/// `LinkBackend` on `lt-remote` through `lt-ffi`.
public struct RustLinkBackend: LinkBackend {
    /// `false`: listen on loopback only (tests).
    let allInterfaces: Bool

    public init(allInterfaces: Bool = true) { self.allInterfaces = allInterfaces }

    public var defaultPort: UInt16 { remoteDefaultPort() }

    public func start(stateDir: URL, name: String, port: UInt16) throws -> any LinkServer {
        RustLinkServer(try RemoteServer.start(stateDir: stateDir.path, name: name, port: port, allInterfaces: allInterfaces))
    }

    public func devices(stateDir: URL) throws -> [LinkedDevice] {
        try remoteDevices(stateDir: stateDir.path).map {
            LinkedDevice(fingerprint: $0.fingerprint, shortFingerprint: $0.shortFingerprint, name: $0.name,
                         pairedAt: Date(timeIntervalSince1970: Double($0.pairedMs) / 1000), write: $0.write)
        }
    }

    public func revoke(stateDir: URL, fingerprint: String) throws {
        _ = try remoteRevoke(stateDir: stateDir.path, fingerprint: fingerprint)
    }

    public func setWrite(stateDir: URL, fingerprint: String, write: Bool) throws {
        _ = try remoteSetWrite(stateDir: stateDir.path, fingerprint: fingerprint, write: write)
    }
}

final class RustLinkServer: LinkServer, @unchecked Sendable {
    private let server: RemoteServer

    init(_ server: RemoteServer) { self.server = server }

    var port: UInt16 { server.port() }
    var fingerprint: String { server.fingerprint() }

    func serve(_ session: LinkSession?) {
        server.setOpenSession(handle: session?.handle as? SessionHandle, name: session?.name ?? "")
    }

    func beginPairing(host: String) -> LinkPairing {
        let p = server.beginPairing(host: host)
        return LinkPairing(code: p.code, qrPayload: p.qrPayload, expiresIn: TimeInterval(p.expiresInSecs))
    }

    func cancelPairing() { server.cancelPairing() }

    func status() -> LinkStatus {
        let s = server.status()
        return LinkStatus(connections: Int(s.connections), transferring: s.transferring,
                          bytesSent: s.bytesSent, bytesReceived: s.bytesReceived)
    }

    func takeChangedSessions() -> [String] { server.takeChangedSessions() }

    func stop() { server.stop() }
}
