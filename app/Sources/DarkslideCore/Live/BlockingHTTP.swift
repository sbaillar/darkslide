import DarkslideFFI
import Foundation

/// One blocking request over `URLSession`, shared by the Rust HTTP seams
/// (`URLSessionHTTP` for Google Photos, `URLSessionImmichHTTP`, `URLSessionArchiveHTTP`),
/// which Rust calls on a worker thread:
/// - only the URL schemes (and, for plain http, hosts) the caller's `Schemes` allow;
/// - redirects are refused (the 3xx comes back as the answer), so a key, a bearer token
///   or a signed request never follows one to another host;
/// - no cookies are sent or kept;
/// - a request other than GET or HEAD always carries a body, an empty one included, so
///   it goes out with `Content-Length: 0` (S3 answers 411 to a `PUT` without one).
public struct BlockingHTTP: Sendable {
    /// Which URLs a caller may send to.
    public enum Schemes: Sendable {
        case httpsOnly
        /// https, or plain http to any host (Immich on the local network often has no TLS).
        case httpsOrHTTP
        /// https, or plain http to a host on this network only (`allowsPlainHTTP`).
        case httpsOrLocalHTTP
    }

    public struct Answer: Sendable {
        public var status: UInt16
        public var headers: [(name: String, value: String)]
        public var body: Data
    }

    let session: URLSession
    let schemes: Schemes
    let timeout: TimeInterval

    public init(session: URLSession, schemes: Schemes, timeout: TimeInterval) {
        self.session = session
        self.schemes = schemes
        self.timeout = timeout
    }

    /// An ephemeral session that keeps no cookies and no cache.
    public static func makeSession() -> URLSession {
        let c = URLSessionConfiguration.ephemeral
        c.httpCookieStorage = nil
        c.httpShouldSetCookies = false
        c.httpCookieAcceptPolicy = .never
        c.urlCache = nil
        return URLSession(configuration: c)
    }

    /// Whether plain http may go to `host`: a loopback, private (RFC 1918) or link-local
    /// IPv4 address, `localhost`, a `.local` name or a single-label name (ds-archive's
    /// `plain_http_allowed`).
    static func allowsPlainHTTP(host: String) -> Bool {
        let name = host.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]."))
        if name.isEmpty { return false }
        if name.contains(":") { return name == "::1" }
        let parts = name.split(separator: ".", omittingEmptySubsequences: false)
        if parts.allSatisfy({ !$0.isEmpty && $0.allSatisfy(\.isASCII) && $0.allSatisfy(\.isNumber) }) {
            let octets = parts.compactMap { UInt8($0) }
            guard parts.count == 4, octets.count == 4 else { return false }
            return octets[0] == 127 || octets[0] == 10 || (octets[0] == 172 && (16...31).contains(octets[1]))
                || (octets[0] == 192 && octets[1] == 168) || (octets[0] == 169 && octets[1] == 254)
        }
        return name == "localhost" || parts.count == 1 || parts.last == "local" || parts.last == "localhost"
    }

    /// Why `url` may not be requested, in words (nil: it may).
    func problem(_ url: URL, raw: String) -> String? {
        guard let scheme = url.scheme?.lowercased(), let host = url.host, !host.isEmpty else {
            return "bad URL \(raw)"
        }
        switch (scheme, schemes) {
        case ("https", _), ("http", .httpsOrHTTP): return nil
        case ("http", .httpsOrLocalHTTP) where Self.allowsPlainHTTP(host: host): return nil
        case ("http", .httpsOrLocalHTTP):
            return "http:// is only for a server on this network; use https:// for \(host)"
        default: return "bad URL \(raw)"
        }
    }

    /// The `URLRequest` for one call (checked by `problem` first).
    func request(method: String, url: URL, headers: [(String, String)], body: Data) -> URLRequest {
        var r = URLRequest(url: url, timeoutInterval: timeout)
        r.httpMethod = method
        r.httpShouldHandleCookies = false
        for (name, value) in headers { r.setValue(value, forHTTPHeaderField: name) }
        if method != "GET" && method != "HEAD" { r.httpBody = body }
        return r
    }

    /// Send one request and wait for the answer. A disallowed URL is `InvalidArgument`
    /// (nothing is sent); no answer is `Io`.
    public func send(method: String, url raw: String, headers: [(String, String)], body: Data) throws -> Answer {
        guard let url = URL(string: raw) else { throw DsError.InvalidArgument(message: "bad URL \(raw)") }
        if let why = problem(url, raw: raw) { throw DsError.InvalidArgument(message: why) }
        let box = AnswerBox()
        let done = DispatchSemaphore(value: 0)
        let task = session.dataTask(with: request(method: method, url: url, headers: headers, body: body)) { d, r, e in
            box.set(d, r, e)
            done.signal()
        }
        task.delegate = NoRedirects.shared
        task.resume()
        done.wait()
        let (data, response, error) = box.get()
        guard let http = response as? HTTPURLResponse else {
            throw DsError.Io(message: error?.localizedDescription ?? "no answer")
        }
        let headers = http.allHeaderFields.compactMap { k, v in (k as? String).map { (name: $0, value: "\(v)") } }
        return Answer(status: UInt16(clamping: http.statusCode), headers: headers, body: data ?? Data())
    }
}

private final class AnswerBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value: (Data?, URLResponse?, Error?) = (nil, nil, nil)
    func set(_ d: Data?, _ r: URLResponse?, _ e: Error?) { lock.withLock { value = (d, r, e) } }
    func get() -> (Data?, URLResponse?, Error?) { lock.withLock { value } }
}

/// Refuses every redirect: the redirect response itself completes the task.
final class NoRedirects: NSObject, URLSessionTaskDelegate, Sendable {
    static let shared = NoRedirects()

    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}
