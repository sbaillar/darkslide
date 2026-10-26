import LuxtableFFI
import Foundation

/// Immich for Settings and the browser (`RustImmichService` in the app; a fake in
/// tests). Only album membership and library scans are ever written to the server.
public protocol ImmichService: Sendable {
    func testConnection(server: ImmichServer) async throws -> ImmichConnection
    func detectMapping(server: ImmichServer, archiveRoot: String, knownFile: String?) async throws -> ImmichDetectedMapping?
    /// Sync the rule albums for the sessions archived since the last sync (`every`: all
    /// the archive knows), after asking Immich to rescan.
    func syncAlbums(server: ImmichServer, mapping: ImmichPathMapping, rules: [ImmichRule], every: Bool) async throws
        -> ImmichSyncReport
    /// What every archive run does (nil: off).
    func setPublish(_ config: ImmichPublishConfig?) async
    /// Sessions archived since the last album sync.
    func pendingAlbumSyncs() async -> Int

    func mark(_ ids: [PhotoID], developed: Bool, in session: SessionInfo) async throws -> ImmichMarkReport
    func developed(in session: SessionInfo, options: ImmichDevelopOptions) async throws -> [ImmichDeveloped]
    func plan(in session: SessionInfo, options: ImmichDevelopOptions) async throws -> ImmichDevelopPlan
    func rendered(_ job: ImmichDevelopJob, staged: URL, in session: SessionInfo) async throws -> URL?
}

/// `ImmichService` over lt-immich (`immich` in lt-ffi). Requests go out through
/// `URLSessionImmichHTTP`; sessions are the archive's (`ArchiveManager`).
public struct RustImmichService: ImmichService {
    public typealias HandleLookup = @Sendable (SessionInfo) async -> SessionHandle?
    let http: any ImmichHttp
    let manager: ArchiveManager?
    let handles: HandleLookup
    /// What Luxtable added to which album (the app's support folder).
    let statePath: URL

    public init(http: any ImmichHttp = URLSessionImmichHTTP(), manager: ArchiveManager?, handles: @escaping HandleLookup,
                statePath: URL = AppPaths.support.appendingPathComponent("immich-albums.json")) {
        self.http = http
        self.manager = manager
        self.handles = handles
        self.statePath = statePath
    }

    public func testConnection(server: ImmichServer) async throws -> ImmichConnection {
        try await Self.run { token in try await immichTestConnection(http: http, server: server, token: token) }
    }

    public func detectMapping(server: ImmichServer, archiveRoot: String, knownFile: String?) async throws
        -> ImmichDetectedMapping? {
        try await Self.run { token in
            try await immichDetectMapping(http: http, server: server, archiveRoot: archiveRoot, knownFile: knownFile,
                                          token: token)
        }
    }

    public func syncAlbums(server: ImmichServer, mapping: ImmichPathMapping, rules: [ImmichRule], every: Bool) async throws
        -> ImmichSyncReport {
        guard let manager else { throw ImmichError.failed("Archiving isn't running.") }
        try? FileManager.default.createDirectory(at: statePath.deletingLastPathComponent(), withIntermediateDirectories: true)
        let path = statePath.path
        return try await Self.run { token in
            try await immichSyncAlbums(http: http, server: server, mapping: mapping, rules: rules, manager: manager,
                                       statePath: path, scan: true, everySession: every, token: token)
        }
    }

    public func setPublish(_ config: ImmichPublishConfig?) async {
        try? manager?.setImmich(config: config)
    }

    public func pendingAlbumSyncs() async -> Int {
        Int((try? manager?.immichPending()) ?? 0)
    }

    private func handle(_ session: SessionInfo) async throws -> SessionHandle {
        guard let h = await handles(session) else { throw ImmichError.failed("“\(session.name)” isn't open.") }
        return h
    }

    public func mark(_ ids: [PhotoID], developed: Bool, in session: SessionInfo) async throws -> ImmichMarkReport {
        let h = try await handle(session)
        let raw = ids.map(\.raw)
        return try await Self.off { try h.immichMarkDeveloped(ids: raw, on: developed) }
    }

    public func developed(in session: SessionInfo, options: ImmichDevelopOptions) async throws -> [ImmichDeveloped] {
        let h = try await handle(session)
        return try await Self.off { try h.immichDeveloped(options: options) }
    }

    public func plan(in session: SessionInfo, options: ImmichDevelopOptions) async throws -> ImmichDevelopPlan {
        let h = try await handle(session)
        return try await Self.off { try h.immichDevelopPlan(options: options) }
    }

    public func rendered(_ job: ImmichDevelopJob, staged: URL, in session: SessionInfo) async throws -> URL? {
        let h = try await handle(session)
        return try await Self.off { try h.immichDevelopedRendered(job: job, staged: staged.path) }
            .map { URL(fileURLWithPath: $0) }
    }

    /// A blocking FFI call off the main thread; errors in words.
    static func off<T: Sendable>(_ body: @escaping @Sendable () throws -> T) async throws -> T {
        try await Task.detached(priority: .utility) {
            do { return try body() } catch let e as DsError { throw error(e) }
        }.value
    }

    /// An async FFI call with a cancel token tied to the task; errors in words.
    static func run<T>(_ body: (CancelToken) async throws -> T) async throws -> T {
        let token = try CancelToken()
        do {
            return try await withTaskCancellationHandler { try await body(token) } onCancel: { try? token.cancel() }
        } catch let e as DsError {
            throw error(e)
        }
    }

    static func error(_ e: DsError) -> Error {
        switch e {
        case .Cancelled: CancellationError()
        case .Unsupported(let m): ImmichError.notArchivable(m)
        default: ImmichError.failed(RenderError(e).message)
        }
    }
}

/// `ImmichHttp` over `BlockingHTTP` (Rust calls it on a worker thread): http (Immich on
/// the local network often has no TLS) and https; redirects refused, so the API key never
/// follows one to another host.
public final class URLSessionImmichHTTP: ImmichHttp, @unchecked Sendable {
    let http: BlockingHTTP

    public init(session: URLSession = BlockingHTTP.makeSession()) {
        http = BlockingHTTP(session: session, schemes: .httpsOrHTTP, timeout: 60)
    }

    public func send(request: ImmichHttpRequest) throws -> ImmichHttpResponse {
        let a = try http.send(method: request.method, url: request.url, headers: request.headers.map { ($0.name, $0.value) },
                              body: request.body)
        return ImmichHttpResponse(status: a.status, headers: [], body: a.body)
    }
}
