import CryptoKit
import Foundation
import Network

/// PKCE (RFC 7636): a random verifier and its S256 challenge.
public struct PKCE: Sendable, Equatable {
    public let verifier: String

    /// 32 random bytes → a 43-character verifier.
    public init(verifier: String = PKCE.randomString(bytes: 32)) {
        self.verifier = verifier
    }

    public var challenge: String { Self.base64URL(Data(SHA256.hash(data: Data(verifier.utf8)))) }

    public static func randomString(bytes: Int) -> String {
        var b = [UInt8](repeating: 0, count: bytes)
        let ok = SecRandomCopyBytes(kSecRandomDefault, bytes, &b) == errSecSuccess
        precondition(ok, "no system randomness")
        return base64URL(Data(b))
    }

    static func base64URL(_ d: Data) -> String {
        d.base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}

/// Where the consent page is shown (`ASWebAuthenticationSession` in the app).
@MainActor
public protocol GooglePhotosAuthPresenter: AnyObject {
    /// Show `url`; call `onClose` if the user closes the window before finishing.
    func present(_ url: URL, onClose: @escaping @Sendable () -> Void)
    func dismiss()
}

/// Receives the browser's redirect after consent.
public protocol LoopbackReceiving: AnyObject, Sendable {
    /// Start listening on 127.0.0.1; returns the port.
    func start() async throws -> UInt16
    /// The next redirect's request target (`/?code=…&state=…`), in arrival order.
    func target() async throws -> String
    func stop()
}

/// Listens on 127.0.0.1 at a free port for the request the browser makes after the
/// consent page (Google's loopback redirect for Desktop clients), answers it with a
/// "you can close this window" page, and hands request targets over in order (any
/// local client can call the port, so the caller checks each one's state). A connection
/// that hasn't sent its request within `idleTimeout` is closed, and at most `maxQueued`
/// targets wait to be taken (later ones are dropped), so a local client can't pile up either.
public final class LoopbackReceiver: LoopbackReceiving, @unchecked Sendable {
    static let maxQueued = 16

    private let listener: NWListener
    private let idleTimeout: TimeInterval
    private let queue = DispatchQueue(label: "darkslide.google-photos.loopback")
    private let lock = NSLock()
    private var waiting: CheckedContinuation<String, Error>?
    private var received: [String] = []
    private var stopped = false

    public init(idleTimeout: TimeInterval = 10) throws {
        self.idleTimeout = idleTimeout
        let p = NWParameters.tcp
        p.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: .any)
        listener = try NWListener(using: p)
    }

    public func start() async throws -> UInt16 {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<UInt16, Error>) in
            let once = Once()
            listener.stateUpdateHandler = { [listener] state in
                switch state {
                case .ready: if once.claim() { cont.resume(returning: listener.port?.rawValue ?? 0) }
                case .failed(let e): if once.claim() { cont.resume(throwing: e) }
                case .cancelled: if once.claim() { cont.resume(throwing: CancellationError()) }
                default: break
                }
            }
            listener.newConnectionHandler = { [weak self] c in self?.serve(c) }
            listener.start(queue: queue)
        }
    }

    public func target() async throws -> String {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (cont: CheckedContinuation<String, Error>) in
                lock.lock()
                if !received.isEmpty {
                    let r = received.removeFirst()
                    lock.unlock()
                    cont.resume(returning: r)
                } else if stopped {
                    lock.unlock()
                    cont.resume(throwing: CancellationError())
                } else {
                    waiting = cont
                    lock.unlock()
                }
            }
        } onCancel: { self.stop() }
    }

    public func stop() {
        listener.cancel()
        lock.lock()
        stopped = true
        let w = waiting
        waiting = nil
        lock.unlock()
        w?.resume(throwing: CancellationError())
    }

    private func serve(_ c: NWConnection) {
        c.start(queue: queue)
        queue.asyncAfter(deadline: .now() + idleTimeout) { [weak c] in c?.cancel() }
        receive(c, Data())
    }

    private func receive(_ c: NWConnection, _ buffer: Data) {
        c.receive(minimumIncompleteLength: 1, maximumLength: 16_384) { [weak self] data, _, done, error in
            var buf = buffer
            if let data { buf.append(data) }
            if let target = Self.requestTarget(buf) {
                self?.answer(c, target: target)
            } else if error != nil || done || buf.count > 65_536 {
                c.cancel()
            } else {
                self?.receive(c, buf)
            }
        }
    }

    private func answer(_ c: NWConnection, target: String) {
        let isRedirect = Self.isRedirect(target)
        let body = isRedirect ? Self.page : ""
        let head = "HTTP/1.1 \(isRedirect ? "200 OK" : "404 Not Found")\r\nContent-Type: text/html; charset=utf-8\r\n"
            + "Content-Length: \(body.utf8.count)\r\nConnection: close\r\n\r\n"
        c.send(content: Data((head + body).utf8), completion: .contentProcessed { _ in c.cancel() })
        guard isRedirect else { return }
        lock.lock()
        if let w = waiting {
            waiting = nil
            lock.unlock()
            w.resume(returning: target)
        } else {
            if received.count < Self.maxQueued { received.append(target) }
            lock.unlock()
        }
    }

    /// The target of a complete request line (`GET /?code=… HTTP/1.1`), or nil.
    static func requestTarget(_ head: Data) -> String? {
        guard let text = String(data: head, encoding: .utf8), let end = text.range(of: "\r\n") else { return nil }
        let parts = text[..<end.lowerBound].split(separator: " ")
        guard parts.count == 3, parts[2].hasPrefix("HTTP/") else { return "" }
        return String(parts[1])
    }

    /// The redirect is a request for `/` with a query (anything else, e.g. a favicon, isn't).
    static func isRedirect(_ target: String) -> Bool { target.hasPrefix("/?") }

    static let page = """
        <!doctype html><html><head><meta charset="utf-8"><title>Darkslide</title></head>
        <body style="font: 15px -apple-system, sans-serif; text-align: center; margin-top: 15vh">
        <h2>Done</h2><p>You can close this window and go back to Darkslide.</p></body></html>
        """
}

/// Signs in to Google Photos: a loopback listener, the consent page with PKCE, the
/// code exchanged for a refresh token, which goes into the Keychain.
@MainActor
public struct GooglePhotosSignIn {
    let settings: GooglePhotosSettings
    let service: any GooglePhotosService
    let presenter: any GooglePhotosAuthPresenter
    let makeReceiver: () throws -> any LoopbackReceiving

    public init(settings: GooglePhotosSettings, service: any GooglePhotosService, presenter: any GooglePhotosAuthPresenter,
                makeReceiver: @escaping () throws -> any LoopbackReceiving = { try LoopbackReceiver() }) {
        self.settings = settings
        self.service = service
        self.presenter = presenter
        self.makeReceiver = makeReceiver
    }

    /// Throws `CancellationError` when the user closes the consent window.
    public func run() async throws {
        guard settings.isConfigured else { throw GooglePhotosError.notSetUp }
        let receiver = try makeReceiver()
        defer { receiver.stop() }
        let port = try await receiver.start()
        let redirect = "http://127.0.0.1:\(port)/"
        let pkce = PKCE()
        let state = PKCE.randomString(bytes: 16)
        let url = try service.authorizationURL(redirectURI: redirect, challenge: pkce.challenge, state: state)
        presenter.present(url) { receiver.stop() }
        defer { presenter.dismiss() }
        let code = try await Self.code(from: receiver, service: service, state: state)
        let refresh = try await service.exchange(code: code, verifier: pkce.verifier, redirectURI: redirect)
        guard settings.refreshTokenItem.write(refresh) else { throw GooglePhotosError.keychain }
    }

    /// The code from the first redirect carrying our `state`. Any local client can call
    /// the port, so a redirect with another state is ignored (it must not end the
    /// sign-in); one with ours is Google's answer, a refusal included.
    static func code(from receiver: any LoopbackReceiving, service: any GooglePhotosService,
                     state: String) async throws -> String {
        while true {
            let target = try await receiver.target()
            do {
                return try service.code(fromRedirect: target, state: state)
            } catch {
                let theirs = URLComponents(string: target)?.queryItems?.first { $0.name == "state" }?.value
                if theirs == state { throw error }
            }
        }
    }
}

/// Resumes a continuation once.
private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var taken = false
    func claim() -> Bool { lock.withLock { defer { taken = true }; return !taken } }
}
