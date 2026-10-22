import CoreGraphics
import Foundation
import Testing
@testable import DarkslideKit

@Suite struct MockSessionStoreTests {
    @Test func deterministicSessions() async throws {
        let a = MockSessionStore(counts: [2000, 10])
        let sessions = try await a.listSessions()
        #expect(sessions.map(\.photoCount) == [2000, 10])
        let photos = try await a.photos(in: sessions[0])
        #expect(photos.count == 2000)
        #expect(Set(photos.map(\.id)).count == 2000, "unique ids")
        #expect(zip(photos, photos.dropFirst()).allSatisfy { $0.meta.captureDate <= $1.meta.captureDate })
        #expect(photos.contains { $0.meta.isPortrait } && photos.contains { $0.rating > 0 })
        let again = MockSessionStore.makePhotos(count: 5, firstID: 1, start: sessions[0].createdAt)
        #expect(again.map(\.filename) == photos.prefix(5).map(\.filename))
        let albums = try await a.albums(in: sessions[0])
        #expect(albums.count == 2 && albums[0].photoIDs.allSatisfy { id in photos.first { $0.id == id }!.rating >= 4 })
        let names = try await MockSessionStore(counts: [1, 1, 1, 1]).listSessions().map(\.name)
        #expect(names.last == "Session 4")
    }

    @Test func createSaveAndErrors() async throws {
        let s = MockSessionStore(counts: [3], latency: .milliseconds(1))
        let created = try await s.createSession(named: "  New  ")
        #expect(created.name == "New")
        #expect(try await s.listSessions().first?.id == created.id)
        #expect(try await s.photos(in: created).isEmpty)
        await #expect(throws: MockError.invalid("Session name is empty")) { try await s.createSession(named: " ") }
        await #expect(throws: MockError.self) { try await s.createSession(named: "New") }
        let ghost = SessionInfo(name: "ghost", url: URL(fileURLWithPath: "/x"), photoCount: 0, createdAt: Date())
        await #expect(throws: MockError.notFound("session ghost")) { try await s.photos(in: ghost) }
        #expect(try await s.albums(in: ghost).isEmpty)

        let first = try await s.listSessions()[1]
        var p = try await s.photos(in: first)[0]
        p.rating = 5
        try await s.save(photos: [p])
        #expect(try await s.photos(in: first)[0].rating == 5)
        var e = EditState()
        e.exposure.ev = 1
        try await s.save(edits: [p.id: e])
        #expect(try await s.edit(for: p.id) == e)
        #expect(try await s.edit(for: PhotoID(999)) == EditState())
        #expect(await s.saveCount == 2)
        #expect(MockError.notFound("x").errorDescription == "Not found: x")
    }

    @Test func splitMixIsDeterministic() {
        var a = SplitMix64(seed: 42), b = SplitMix64(seed: 42)
        #expect(a.next() == b.next())
        let u = a.unit()
        #expect(u >= 0 && u < 1)
    }
}

@Suite struct MockImageProviderTests {
    @Test func thumbnailsHaveOrientationAndSize() async throws {
        let p = MockImageProvider(latency: .zero)
        let land = try await p.thumbnail(for: samplePhoto(), maxPixelSize: 300)
        #expect(land.width == 300 && land.height == 200)
        let port = try await p.thumbnail(for: samplePhoto(portrait: true), maxPixelSize: 300)
        #expect(port.width == 200 && port.height == 300)
        let tiny = try await p.thumbnail(for: samplePhoto(), maxPixelSize: 1)
        #expect(tiny.width == 16)
    }

    @Test func burstsLookAlikeAndDifferentScenesDont() throws {
        func bytes(_ seed: Int64) throws -> [UInt8] {
            let img = try #require(MockImageProvider.draw(seed: seed, width: 32, height: 21))
            let ctx = CGContext(data: nil, width: 32, height: 21, bitsPerComponent: 8, bytesPerRow: 128,
                                space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
            ctx.draw(img, in: CGRect(x: 0, y: 0, width: 32, height: 21))
            return Array(UnsafeBufferPointer(start: ctx.data!.assumingMemoryBound(to: UInt8.self), count: 128 * 21))
        }
        func dist(_ a: [UInt8], _ b: [UInt8]) -> Int { zip(a, b).reduce(0) { $0 + abs(Int($1.0) - Int($1.1)) } }
        let same = dist(try bytes(8), try bytes(9))
        let other = dist(try bytes(8), try bytes(400))
        #expect(same < other)
    }

    @Test func honoursCancellation() async {
        let p = MockImageProvider(latency: .seconds(5))
        let t = Task { try await p.thumbnail(for: samplePhoto(), maxPixelSize: 64) }
        t.cancel()
        await #expect(throws: CancellationError.self) { try await t.value }
    }
}

@Suite struct MockArchiveServiceTests {
    @Test func syncsAndPublishesProgress() async {
        let a = MockArchiveService(target: "/x", interval: .seconds(3600), step: .milliseconds(1), pending: 3,
                                   isReachable: { _ in true })
        #expect(a.status.state == .behind(files: 3))
        var states: [ArchiveStatus.State] = []
        for await s in a.statusUpdates() {
            states.append(s.state)
            if case .idle = s.state { break }
        }
        #expect(states.first == .behind(files: 3))
        #expect(states.contains(.syncing(fraction: 1)))
        #expect(a.status.lastSuccess != nil)
    }

    @Test func offlineWhenTargetMissing() async {
        let a = MockArchiveService(target: "/Volumes/definitely-not-mounted-\(UUID())/ds", pending: 0)
        #expect(a.status.state == .idle)
        await a.syncNow()
        #expect(a.status.state == .offline)
        #expect(!MockArchiveService.volumeMounted("/Volumes/nope-\(UUID())/x"))
        #expect(MockArchiveService.volumeMounted(NSTemporaryDirectory() + "sub"))
    }
}

@Suite struct MockAIServiceTests {
    @Test func deterministicCull() async throws {
        let photos = MockSessionStore.makePhotos(count: 30, firstID: 1, start: Date())
        let progress = EventCounter()
        let s = try await MockAIService().cull(photos, images: MockImageProvider(latency: .zero)) { _, _ in progress.increment() }
        #expect(s.count == 30 && progress.value == 30)
        #expect(s.allSatisfy { ($0.score > 0.8) == ($0.pick == .picked) })
        let again = try await MockAIService().cull(photos, images: MockImageProvider(latency: .zero)) { _, _ in }
        #expect(again == s)
        let e = try await MockAIService().autoAdjust(photos[0], current: EditState(), images: MockImageProvider())
        #expect(!e.whiteBalance.asShot)
        #expect(try await MockAIService().applyInstruction("x", to: e) == e)
    }
}
