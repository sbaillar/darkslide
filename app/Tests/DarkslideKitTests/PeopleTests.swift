import CoreGraphics
import DarkslideFFI
import Foundation
import ImageIO
import Testing
@testable import DarkslideKit

/// Faces by image width: width 2 has one face, 3 has two, anything else none.
struct FakeFaceEmbedder: FaceEmbedding {
    var modelID = "fake-faces"
    func faces(in image: CGImage) throws -> [FoundFace] {
        let f = { (x: CGFloat) in FoundFace(rect: CGRect(x: x, y: 0.1, width: 0.2, height: 0.2), quality: 0.7, vector: [1, 0]) }
        switch image.width {
        case 2: return [f(0.1)]
        case 3: return [f(0.1), f(0.6)]
        default: return []
        }
    }
}

func faceRecord(_ key: String, _ idx: UInt32 = 0, variant: Int64) -> FaceRecord {
    FaceRecord(id: FaceId(key: key, idx: idx), rect: FaceRect(x: 0.1, y: 0.1, w: 0.2, h: 0.2), quality: 0.5,
               path: "/x/\(key).ARW", variantId: variant)
}

/// In-memory `PeopleBackend`.
final class FakePeopleBackend: PeopleBackend, @unchecked Sendable {
    struct State {
        var jobs: [SearchJob] = []
        var stored: [String: [FoundFace]] = [:]
        var locationsLeft = 0
        var locationCalls = 0
        var waiting = 0
        var polls = 0
        var syncs = 0
        var overview = PeopleOverview(people: [], unnamed: [], status: FaceStatus(scanned: 0, total: 0, waiting: 0, faces: 0))
        var known: [KnownPerson] = []
        var calls: [String] = []
        var named: [([FaceId], String)] = []
        var rejected: [([FaceId], String?, String)] = []
        var merged: [[FaceId]] = []
        var split: [([FaceId], [FaceId])] = []
        var renamed: [(String, String)] = []
        var renameResult = true
        var fail: Error?
        var failStore = false
    }

    let state = SearchLocked(State())

    func faceJobs(model: String, in session: SessionInfo) async throws -> [SearchJob] {
        try state.withLock { s in
            if let e = s.fail { throw e }
            return s.jobs.filter { s.stored[$0.key] == nil }
        }
    }

    func storeFaces(_ faces: [FoundFace], key: String, model: String, in session: SessionInfo) async throws {
        try state.withLock { s in
            if s.failStore { throw SearchBackendError.sessionNotOpen }
            s.stored[key] = faces
        }
    }

    func faceStatus(model: String, in session: SessionInfo) async throws -> FaceStatus {
        state.withLock { s in
            s.polls += 1
            let waiting = s.polls > 1 ? 0 : s.waiting
            return FaceStatus(scanned: UInt64(s.stored.count), total: UInt64(s.jobs.count), waiting: UInt64(waiting),
                              faces: UInt64(s.stored.values.map(\.count).reduce(0, +)))
        }
    }

    func overview(model: String, tuning: PeopleTuning, in session: SessionInfo) async throws -> PeopleOverview {
        try state.withLock { s in
            if let e = s.fail { throw e }
            s.calls.append("overview")
            return s.overview
        }
    }

    func name(_ faces: [FaceId], as name: String, model: String, in session: SessionInfo) async throws -> KnownPerson {
        try state.withLock { s in
            if let e = s.fail { throw e }
            s.named.append((faces, name))
            return KnownPerson(id: "p-\(name)", name: name, aliases: [])
        }
    }

    func reject(_ faces: [FaceId], personID: String?, name: String, model: String, in session: SessionInfo) async throws {
        state.withLock { $0.rejected.append((faces, personID, name)) }
    }

    func merge(_ faces: [FaceId], in session: SessionInfo) async throws {
        state.withLock { $0.merged.append(faces) }
    }

    func split(_ faces: [FaceId], from rest: [FaceId], in session: SessionInfo) async throws {
        state.withLock { $0.split.append((faces, rest)) }
    }

    func rename(personID: String, to name: String, in session: SessionInfo?) async throws -> Bool {
        state.withLock { s in
            s.renamed.append((personID, name))
            return s.renameResult
        }
    }

    func knownPeople() async throws -> [KnownPerson] { state.get.known }

    func syncFind(faceModel: String, suggest: Float, in session: SessionInfo) async throws {
        state.withLock { $0.syncs += 1 }
    }

    func scanLocations(limit: Int, in session: SessionInfo) async throws -> Int {
        state.withLock { s in
            s.locationCalls += 1
            s.locationsLeft = max(0, s.locationsLeft - limit)
            return s.locationsLeft
        }
    }
}

@MainActor
@Suite(.serialized) struct PeopleTests {
    let session = SessionInfo(id: UUID(), name: "S", url: URL(fileURLWithPath: "/tmp/S"), photoCount: 0, createdAt: Date())

    func indexer(_ b: FakePeopleBackend, active: @escaping @Sendable () -> Bool = { false }) -> FaceIndexer {
        var i = FaceIndexer(backend: b, embedder: FakeFaceEmbedder(), preview: { url in
            if url.lastPathComponent == "bad.ARW" { throw SearchBackendError.sessionNotOpen }
            return tinyImage(width: url.lastPathComponent.hasPrefix("two") ? 3 : url.lastPathComponent.hasPrefix("one") ? 2 : 1)
        }, isUserActive: active)
        i.waitingPoll = .milliseconds(1)
        i.idlePoll = .milliseconds(1)
        i.syncEvery = 2
        i.locationBatch = 4
        return i
    }

    @Test func indexerReadsPlacesThenFacesAndSyncsFind() async throws {
        let b = FakePeopleBackend()
        b.state.withLock { s in
            s.locationsLeft = 10
            s.waiting = 1
            s.jobs = ["one.ARW", "two.ARW", "none.ARW", "bad.ARW"].enumerated().map {
                SearchJob(key: "k\($0.offset)", url: URL(fileURLWithPath: "/x/\($0.element)"))
            }
        }
        let progress = SearchLocked<[FaceIndexer.Progress]>([])
        let report = try await indexer(b).run(session) { p in progress.withLock { $0.append(p) } }
        #expect(report == FaceIndexer.Report(scanned: 3, faces: 3, failed: 1))
        let s = b.state.get
        #expect(s.locationCalls == 3, "10 places, 4 at a time")
        #expect(s.stored["k1"]?.count == 2 && s.stored["k2"]?.isEmpty == true)
        #expect(s.syncs >= 2, "every 2 photos and at the end")
        #expect(s.polls >= 2, "waited for a photo still being identified")
        #expect(progress.get.last == FaceIndexer.Progress(done: 4, total: 4))
        // Next run: only the unreadable photo is tried again; it still syncs once.
        let syncs = b.state.get.syncs
        let again = try await indexer(b).run(session)
        #expect(again == FaceIndexer.Report(scanned: 0, faces: 0, failed: 1))
        #expect(b.state.get.syncs == syncs + 1)
    }

    @Test func indexerWaitsForTheUserAndStopsWhenCancelled() async throws {
        let b = FakePeopleBackend()
        b.state.withLock { $0.jobs = [SearchJob(key: "k", url: URL(fileURLWithPath: "/x/one.ARW"))] }
        let busy = SearchLocked(true)
        let task = Task { try await indexer(b, active: { busy.get }).run(session) }
        try await Task.sleep(for: .milliseconds(30))
        #expect(b.state.get.stored.isEmpty, "paused while culling")
        task.cancel()
        await #expect(throws: CancellationError.self) { try await task.value }
        // A store that fails counts as failed, not fatal.
        b.state.withLock { $0.failStore = true }
        let r = try await indexer(b).run(session)
        #expect(r.failed == 1)
        #expect(FaceIndexer.previewEdge == 1616)
    }

    func model(_ b: FakePeopleBackend) async -> AppModel {
        let m = makeModel(counts: [6, 2])
        m.people.configure(backend: b)
        m.people.makeEmbedder = { FakeFaceEmbedder() }
        m.people.makeIndexer = { backend, e in
            var i = FaceIndexer(backend: backend, embedder: e, preview: { _ in tinyImage(width: 2) }, isUserActive: { false })
            i.waitingPoll = .milliseconds(1)
            return i
        }
        await m.start()
        return m
    }

    @Test func overviewTagsPhotosAndAPersonFiltersTheBrowser() async throws {
        let b = FakePeopleBackend()
        let m = await model(b)
        let ids = m.photos.map(\.id)
        let alex = PersonGroup(personId: "p1", name: "Alex", faces: [faceRecord("a", variant: ids[0].raw)], photos: 2,
                                variantIds: [ids[0].raw, ids[3].raw])
        let sam = PersonGroup(personId: nil, name: "Sam", faces: [], photos: 1, variantIds: [ids[3].raw])
        let cluster = FaceCluster(faces: [faceRecord("b", variant: ids[1].raw), faceRecord("c", variant: ids[2].raw)],
                                  suggestion: PersonSuggestion(personId: "p1", name: "Alex", similarity: 0.8))
        b.state.withLock { s in
            s.overview = PeopleOverview(people: [alex, sam], unnamed: [cluster],
                                        status: FaceStatus(scanned: 6, total: 6, waiting: 0, faces: 3))
            s.known = [KnownPerson(id: "p1", name: "Alex", aliases: []), KnownPerson(id: "p2", name: "Alexandra", aliases: [])]
        }
        m.people.startIndexing()
        await m.people.waitForIndexing()
        await m.people.reload()
        #expect(m.people.overview?.people.count == 2)
        #expect(m.photo(ids[3])?.people == ["Alex", "Sam"])
        #expect(m.photo(ids[0])?.people == ["Alex"])
        #expect(m.photo(ids[1])?.people == [])
        #expect(PeopleModel.peopleByPhoto(m.people.overview!)[ids[3]] == ["Alex", "Sam"])
        #expect(m.people.completions(for: "ale") == ["Alex", "Alexandra"])
        #expect(m.people.completions(for: "alex") == ["Alexandra"])
        #expect(m.people.completions(for: " ").isEmpty)
        // A person as a filter (again: everyone).
        m.people.show(alex)
        #expect(m.filter.person == "Alex")
        #expect(Set(m.visibleIDs) == [ids[0], ids[3]])
        m.people.show(alex)
        #expect(m.filter.person == nil)
        #expect(m.visibleIDs.count == 6)
        var f = PhotoFilter()
        f.person = "alex"
        var p = samplePhoto()
        #expect(!f.matches(p))
        p.people = ["ALEX"]
        #expect(f.matches(p) && f.isActive)
    }

    @Test func actionsReachTheBackendAndReload() async throws {
        let b = FakePeopleBackend()
        let m = await model(b)
        let ids = m.photos.map(\.id)
        let c1 = FaceCluster(faces: [faceRecord("a", variant: ids[0].raw), faceRecord("b", variant: ids[1].raw)],
                             suggestion: PersonSuggestion(personId: "p1", name: "Alex", similarity: 0.8))
        let c2 = FaceCluster(faces: [faceRecord("c", variant: ids[2].raw)], suggestion: nil)
        let alex = PersonGroup(personId: "p1", name: "Alex", faces: [faceRecord("d", variant: ids[3].raw)], photos: 1,
                                variantIds: [ids[3].raw])
        b.state.withLock { $0.overview = PeopleOverview(people: [alex], unnamed: [c1, c2],
                                                         status: FaceStatus(scanned: 0, total: 0, waiting: 0, faces: 0)) }
        await m.people.reload()
        let syncs = b.state.get.syncs

        await m.people.name(c2, as: "  Dana ")
        await m.people.name(c2, as: "   ")
        #expect(b.state.get.named.map(\.1) == ["Dana"])
        await m.people.confirm(c1)
        #expect(b.state.get.named.last?.1 == "Alex")
        await m.people.confirm(c2) // no suggestion: nothing
        await m.people.reject(c1)
        await m.people.reject(c2)
        #expect(b.state.get.rejected.count == 1)
        #expect(b.state.get.rejected[0].1 == "p1")
        // Merge needs two selected clusters.
        m.people.selected = [0]
        await m.people.mergeSelected()
        #expect(b.state.get.merged.isEmpty)
        m.people.selected = [0, 1, 7]
        await m.people.mergeSelected()
        #expect(b.state.get.merged.first?.count == 3)
        #expect(m.people.selected.isEmpty)
        await m.people.merge(c2, into: alex)
        #expect(b.state.get.named.last?.1 == "Alex")
        // Split: picked faces off the rest (nothing to split off a single face).
        await m.people.split([c1.faces[0]], from: c1)
        #expect(b.state.get.split.first?.0 == [c1.faces[0].id] && b.state.get.split.first?.1 == [c1.faces[1].id])
        await m.people.split(c2.faces, from: c2)
        #expect(b.state.get.split.count == 1)
        await m.people.remove(alex.faces, from: alex)
        await m.people.remove([], from: alex)
        #expect(b.state.get.rejected.last?.2 == "Alex")
        // Rename (and a filter on the old name follows); a taken name is an error.
        m.people.show(alex)
        await m.people.rename(alex, to: "Alexander")
        #expect(b.state.get.renamed.last! == ("p1", "Alexander"))
        #expect(m.filter.person == "Alexander")
        await m.people.rename(alex, to: "Alex") // unchanged
        #expect(b.state.get.renamed.count == 1)
        b.state.withLock { $0.renameResult = false }
        await m.people.rename(alex, to: "Sam")
        #expect(m.people.error == PeopleError.nameTaken("Sam").errorDescription)
        #expect(b.state.get.syncs > syncs, "every action makes Find catch up")
        // Failures are shown, not thrown.
        b.state.withLock { $0.fail = SearchBackendError.sessionNotOpen }
        await m.people.name(c2, as: "X")
        #expect(m.people.error == SearchBackendError.sessionNotOpen.errorDescription)
        await m.people.reload()
        b.state.withLock { $0.fail = nil }
        await m.people.reload()
        #expect(m.people.error != nil, "kept until the next action")
    }

    @Test func statusAndSessionLifecycle() async throws {
        let b = FakePeopleBackend()
        let m = await model(b)
        #expect(m.people.isAvailable)
        #expect(m.people.indexingMessage == nil)
        m.people.sessionClosed()
        #expect(m.people.overview == nil && m.people.selected.isEmpty)
        let unconfigured = PeopleModel()
        #expect(!unconfigured.isAvailable)
        unconfigured.startIndexing()
        await unconfigured.reload()
        #expect(unconfigured.overview == nil)
        #expect(PeopleModel.defaultTuning.record.clusterThreshold == 0.70)
        #expect(PeopleModel.defaultTuning.autoMatch > PeopleModel.defaultTuning.suggest, "Find needs more than a question")
        #expect(AppPaths.peopleStore.lastPathComponent == "people.json")
    }

    /// Budget: face scanning a 1616 px A9 preview (decode + detect + embed)
    /// stays ≤ 60 ms per photo, fastest of 7 (measured alone: 11 ms; detection
    /// 7–8 ms without faces, 15 ms with one face embedded; load-scaled), so a
    /// background pass of a card is minutes, paused while the user culls.
    @Test func faceScanningAPreviewIsWithinBudget() async throws {
        guard let sample = sampleARWs().first else {
            print("SKIP: faceScanningAPreviewIsWithinBudget — no A9 samples")
            return
        }
        let e = VisionFaceEmbedder()
        _ = try e.faces(in: try await FaceIndexer.embeddedPreview(sample)) // warm Vision
        var times: [Duration] = []
        for _ in 0..<7 {
            let clock = ContinuousClock()
            let t0 = clock.now
            let image = try await FaceIndexer.embeddedPreview(sample)
            #expect(max(image.width, image.height) == FaceIndexer.previewEdge)
            #expect(try e.faces(in: image).isEmpty, "the samples have no visible faces")
            times.append(clock.now - t0)
        }
        // The fastest run: the cost itself (the full suite runs Vision in parallel
        // tests, so medians there measure contention, 300 ms seen under load).
        let best = times.min()!
        print("face scan per photo: best \(best), median \(times.sorted()[3])")
        #expect(best < .milliseconds(60) * TestTiming.loadFactor, "\(best)")
    }

    /// Clustering-quality harness (skipped unless `DS_FACE_EVAL_DIR` is set):
    /// `<dir>/<person>/*.jpg` labeled photos → the largest face of each, embedded
    /// like the app does, written as `label<TAB>v1,v2,…` to `DS_FACE_EVAL_OUT`
    /// (default `<dir>/faces.tsv`). Score it with
    /// `DS_FACE_EVAL_TSV=… cargo test -p ds-ai faces::tests::eval_harness -- --nocapture`.
    @Test func faceEvalHarness() throws {
        let env = ProcessInfo.processInfo.environment
        guard let dir = env["DS_FACE_EVAL_DIR"].map({ URL(fileURLWithPath: $0) }) else { return }
        let out = env["DS_FACE_EVAL_OUT"].map { URL(fileURLWithPath: $0) } ?? dir.appendingPathComponent("faces.tsv")
        let e = VisionFaceEmbedder()
        var lines: [String] = []
        let fm = FileManager.default
        for person in try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: [.isDirectoryKey]).sorted(by: { $0.path < $1.path }) {
            guard (try? person.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true else { continue }
            for file in try fm.contentsOfDirectory(at: person, includingPropertiesForKeys: nil).sorted(by: { $0.path < $1.path }) {
                guard let src = CGImageSourceCreateWithURL(file as CFURL, nil),
                      let img = CGImageSourceCreateThumbnailAtIndex(src, 0, [kCGImageSourceCreateThumbnailFromImageAlways: true,
                                                                             kCGImageSourceThumbnailMaxPixelSize: 1616,
                                                                             kCGImageSourceCreateThumbnailWithTransform: true] as CFDictionary),
                      let face = try e.faces(in: img).max(by: { $0.rect.width < $1.rect.width })
                else { continue }
                lines.append(person.lastPathComponent + "\t" + face.vector.map { String($0) }.joined(separator: ","))
            }
        }
        try lines.joined(separator: "\n").write(to: out, atomically: true, encoding: .utf8)
        print("face eval: \(lines.count) faces → \(out.path)")
    }

    /// Vision on an image without anyone in it finds nobody (the samples have
    /// no faces either); on Keynote's stock portrait (when installed) it finds
    /// one face, upright and embedded.
    @Test func visionFindsNoFacesInEmptyImagesAndOneInAPortrait() throws {
        let e = VisionFaceEmbedder()
        #expect(e.modelID == VisionFaceEmbedder.modelID)
        let ctx = CGContext(data: nil, width: 64, height: 48, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        ctx.setFillColor(CGColor(red: 0.2, green: 0.5, blue: 0.8, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: 64, height: 48))
        #expect(try e.faces(in: ctx.makeImage()!).isEmpty)
        let floats = VisionFaceEmbedder.floats(Data([0, 0, 128, 63, 0, 0, 0, 64]), type: .float, count: 2)
        #expect(floats == [1, 2])
        var d = Data(count: 8)
        d.withUnsafeMutableBytes { $0.storeBytes(of: 0.5, as: Double.self) }
        #expect(VisionFaceEmbedder.floats(d, type: .double, count: 1) == [0.5])
        let portrait = URL(fileURLWithPath:
            "/Applications/Keynote.app/Contents/SharedSupport/DocumentResources/a9/cbb46fcbaf55e3c6b44fcf52eccf7780e0b611.jpeg")
        guard let src = CGImageSourceCreateWithURL(portrait as CFURL, nil),
              let img = CGImageSourceCreateThumbnailAtIndex(src, 0, [kCGImageSourceCreateThumbnailFromImageAlways: true,
                                                                     kCGImageSourceThumbnailMaxPixelSize: 1616] as CFDictionary)
        else {
            print("SKIP: vision portrait — Keynote's stock image isn't installed")
            return
        }
        let faces = try e.faces(in: img)
        #expect(faces.count == 1)
        let f = try #require(faces.first)
        #expect(f.vector.count == 768)
        #expect(f.rect.minY > 0 && f.rect.maxY < 1 && f.rect.width > 0.05, "\(f.rect)")
        #expect(f.quality > 0)
    }
}
