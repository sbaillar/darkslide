import CoreGraphics
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// The Library on the real stack: a folder opened in place (A9 samples when
/// present, else empty), synced into a scratch global index through the FFI,
/// queried, edited through its session, collected; the Library mode loads it.
@MainActor
@Suite(.serialized) struct LibraryLiveTests {
    static let all = LibraryQueryRecord(sessions: [], fromMs: nil, toMs: nil, minRating: 0, unrated: false, labels: [],
                                        pick: .all, cameras: [], keywords: [], text: "", collection: nil,
                                        sort: .captured, descending: false)

    @Test func theLibraryOverTheRealIndex() async throws {
        let samples = Array(sampleARWs().prefix(2))
        let live = try await SearchLiveTests.open(samples, name: "Lib Shoot")
        defer { try? FileManager.default.removeItem(at: live.dir) }
        let b = RustLibraryBackend(store: live.store, previews: RustPreviewProvider(),
                                   globalIndex: live.dir.appendingPathComponent("library.db"))
        let m = live.model
        let session = try #require(m.currentSession)
        try await b.sync(session)
        #expect(try await b.refresh() == false, "nothing else changed")
        let page = try await b.query(Self.all, first: 10)
        #expect(page.ids.count == samples.count)
        let o = try await b.overview()
        #expect(o.sessions.map(\.name) == ["Lib Shoot"] && o.total == UInt64(samples.count))
        #expect(try await b.items(page.ids).count == samples.count)

        let c = try await b.createCollection(name: "Picks", rules: nil, ids: page.ids)
        #expect(try await b.members(of: c, page.ids, remove: true) == samples.count)
        #expect(try await b.updateCollection(c, name: "Keepers", rules: nil))
        #expect(try await b.deleteCollection(c))
        let r = try await b.edit(page.ids, .rating(stars: 3))
        #expect(r.changed.count == samples.count && r.failed.isEmpty)
        if let first = page.first.first {
            #expect(try await b.thumbnail(for: first, maxPixelSize: 256).width > 0)
            #expect(first.sessionName == "Lib Shoot")
        }

        // The mode loads it (the open session synced in the background).
        m.library.configure(backend: b)
        m.browserMode = .library
        m.library.appeared()
        await m.library.settled()
        #expect(m.library.count == samples.count)
        #expect(m.library.phase == .idle)
    }
}
