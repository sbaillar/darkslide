import CoreGraphics
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideKit

/// Find and People end to end on the real stack: A9 samples opened in place
/// (ds-session through the FFI), faces stored and named, the people store, the
/// global index, places from an XMP sidecar, and Find across it all. Faces come
/// from a stand-in embedder (the samples have none Vision can see).
@MainActor
@Suite(.serialized) struct FindLiveTests {
    /// Every portrait-orientation preview has one face of the same person.
    struct PortraitFaces: FaceEmbedding {
        var modelID = "portrait-faces"
        func faces(in image: CGImage) throws -> [FoundFace] {
            image.height > image.width
                ? [FoundFace(rect: CGRect(x: 0.4, y: 0.3, width: 0.2, height: 0.15), quality: 0.8, vector: [1, 0.05, 0])]
                : []
        }
    }

    @Test func alexInIcelandFromNamingToFinding() async throws {
        let samples = sampleARWs()
        guard samples.count >= 4 else {
            print("SKIP: alexInIcelandFromNamingToFinding — no A9 samples")
            return
        }
        var dirs: (global: URL, people: URL)?
        let live = try await SearchLiveTests.open(Array(samples.prefix(4)), name: "2024-07 Iceland") { m in
            guard let store = m.sessions as? RustSessionStore else { return }
            let base = FileManager.default.temporaryDirectory.appendingPathComponent("ds-find-\(UUID())", isDirectory: true)
            let d = (global: base.appendingPathComponent("global.db"), people: base.appendingPathComponent("people.json"))
            dirs = d
            m.people.configure(backend: RustPeopleBackend(store: store, globalIndex: d.global, peopleStore: d.people))
            m.people.makeEmbedder = { PortraitFaces() }
            m.people.makeIndexer = { b, e in
                var i = FaceIndexer(backend: b, embedder: e, isUserActive: { false })
                i.waitingPoll = .milliseconds(50)
                return i
            }
            m.find.configure(backend: RustFindBackend(globalIndex: d.global, peopleStore: d.people))
        }
        defer { try? FileManager.default.removeItem(at: live.dir) }
        let paths = try #require(dirs)
        defer { try? FileManager.default.removeItem(at: paths.global.deletingLastPathComponent()) }
        let m = live.model
        let portraits = Set(m.photos.filter { $0.meta.pixelHeight > $0.meta.pixelWidth }.map(\.id))
        #expect(portraits.count == 3)

        // Faces are found in the background (the open started the scan); the portraits
        // make one unnamed group. No second startIndexing(): if the open's scan is
        // already done, a second one finds nothing left and its report says 0 scanned.
        await m.people.waitForIndexing()
        #expect(m.people.lastReport?.scanned == 4)
        #expect(m.people.lastReport?.faces == 3)
        await m.people.reload()
        let cluster = try #require(m.people.overview?.unnamed.first)
        #expect(m.people.overview?.unnamed.count == 1 && cluster.faces.count == 3)
        #expect(cluster.suggestion == nil)

        // Split one face off (someone else), then merge it back.
        await m.people.split([cluster.faces[0]], from: cluster)
        #expect(m.people.overview?.unnamed.map(\.faces.count).sorted() == [1, 2])
        m.people.selected = [0, 1]
        await m.people.mergeSelected()
        #expect(m.people.overview?.unnamed.map(\.faces.count) == [3])
        let merged = try #require(m.people.overview?.unnamed.first)

        // "Who is this?" → Alex: the sidecars, the browser and the people store know.
        await m.people.name(merged, as: "Alex")
        #expect(m.people.error == nil)
        #expect(m.people.overview?.people.map(\.name) == ["Alex"])
        #expect(m.people.overview?.unnamed.isEmpty == true)
        #expect(Set(m.photos.filter { $0.people == ["Alex"] }.map(\.id)) == portraits)
        #expect(try await m.people.backend?.knownPeople().map(\.name) == ["Alex"])
        let alex = try #require(m.people.overview?.people.first)
        m.people.show(alex)
        #expect(Set(m.visibleIDs) == portraits)
        m.people.show(alex)
        // Rename there and back (the store keeps the old name as an alias).
        await m.people.rename(alex, to: "Alexander")
        #expect(m.people.overview?.people.map(\.name) == ["Alexander"])
        let renamed = try #require(m.people.overview?.people.first)
        await m.people.rename(renamed, to: "Alex")
        #expect(try await m.people.backend?.knownPeople().first?.aliases == ["Alexander"])

        // Find: Alex (known) in Iceland (the session's name).
        m.find.query = "let's find pics of alex in iceland"
        await m.find.run()
        let r = try #require(m.find.results)
        #expect(r.photos.count == 3, "\(r.photos.map(\.path))")
        #expect(m.find.summary.map(\.text) == ["Alex", "matched session name “2024-07 Iceland”"])
        #expect(r.sessions.first?.name == "2024-07 Iceland" && r.sessions.first?.hits == 3)
        #expect(FindModel.chips(try #require(r.photos.first), phrase: nil).map(\.text)
            == ["Alex", "matched session name “2024-07 Iceland”"])
        // Nobody named Zephyr: said so, and nothing is filtered on it.
        m.find.query = "pics of Zephyr"
        await m.find.run()
        #expect(m.find.notes.first == "No one named “Zephyr” yet — name faces in People.")
        #expect(m.find.results?.total == 4)

        // Places from an XMP sidecar (Lightroom's), read on the next indexing pass.
        let landscape = try #require(m.photos.first { !portraits.contains($0.id) })
        let xmp = try #require(landscape.url).deletingPathExtension().appendingPathExtension("xmp")
        try #"<x:xmpmeta><rdf:Description photoshop:City="Vik" photoshop:State="Sudurland"/></x:xmpmeta>"#
            .write(to: xmp, atomically: true, encoding: .utf8)
        // Read again because its sidecar is new (places were read on the first pass).
        let session = try #require(m.currentSession)
        let left = try await (m.people.backend as? RustPeopleBackend)?.scanLocations(limit: 10, in: session)
        #expect(left == 0)
        m.find.query = "vik"
        await m.find.run()
        #expect(m.find.results?.photos.map { URL(fileURLWithPath: $0.path).lastPathComponent } == [landscape.filename])
        #expect(m.find.summary.map(\.text) == ["location “Vik”"])

        // "Not Alex" on one face: off Alex, and never suggested there again.
        let named = try #require(m.people.overview?.people.first)
        await m.people.remove([named.faces[0]], from: named)
        #expect(m.people.overview?.people.first?.faces.count == 2)
        #expect(m.people.overview?.unnamed.first?.suggestion == nil)

        // Opening a result shows the photo, even when the browser's filter hides it.
        let target = try #require(r.photos.first)
        m.filter.minRating = 5
        m.find.present()
        await m.find.open(target)
        #expect(m.focusedPhoto?.url?.resolvingSymlinksInPath().path
            == URL(fileURLWithPath: target.path).resolvingSymlinksInPath().path)
        #expect(m.filter == PhotoFilter() && !m.find.isPresented)
        let s = try #require(r.sessions.first)
        m.find.present()
        await m.find.open(s)
        #expect(!m.find.isPresented && m.currentSession != nil)
    }
}
