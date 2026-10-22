import CoreGraphics
import CoreML
import CoreVideo
import DarkslideAI
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// A depth model stand-in: the left half near (1), the right half far (0).
struct HalfDepth: DepthEstimator {
    var id = "fake-depth-1"
    func estimate(_ image: CGImage) throws -> DepthField {
        DepthField(width: 4, height: 2, values: [1, 1, 0, 0, 1, 1, 0, 0])
    }
}

/// A gray image of `w` × `h`.
func grayImage(_ w: Int = 64, _ h: Int = 32) -> CGImage {
    let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue)!
    ctx.setFillColor(gray: 0.5, alpha: 1)
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    return ctx.makeImage()!
}

/// A matte covering the left quarter.
func leftQuarterMatte(_ w: Int = 8, _ h: Int = 4) -> VisionMask {
    VisionMask(width: w, height: h, bytes: (0..<(w * h)).map { $0 % w < w / 4 ? 255 : 0 })
}

@Suite("Lens blur depth maps")
struct DepthMapTests {
    // MARK: Composition

    @Test func modelDepthIsNormalizedAndTheSubjectFlattenedToOnePlane() {
        // Depth ramps 0.2 → 0.6 left to right; the matte covers the left quarter.
        let field = DepthField(width: 8, height: 1, values: (0..<8).map { 0.2 + Float($0) * 0.4 / 7 })
        let m = DepthComposer.compose(depth: field, matte: leftQuarterMatte(8, 1), width: 8, height: 1)
        #expect(m.width == 8 && m.height == 1)
        // Normalized over its range: the far right end is 255; the subject (left two
        // pixels) is one value, the mean of its depth.
        #expect(m.bytes[7] == 255)
        #expect(m.bytes[0] == m.bytes[1])
        #expect(m.bytes[0] < m.bytes[3], "the subject is its own depth, not the background's")
        // Without a matte the ramp is kept as is.
        let plain = DepthComposer.compose(depth: field, matte: nil, width: 8, height: 1)
        #expect(plain.bytes.first == 0 && plain.bytes.last == 255 && plain.bytes == plain.bytes.sorted())
        // A flat field doesn't divide by zero.
        let flat = DepthComposer.compose(depth: DepthField(width: 2, height: 1, values: [0.5, 0.5]), matte: nil, width: 2, height: 1)
        #expect(flat.bytes == [0, 0])
        // An empty matte flattens nothing.
        let none = DepthComposer.compose(depth: field, matte: VisionMask(width: 8, height: 1, bytes: Array(repeating: 0, count: 8)),
                                         width: 8, height: 1)
        #expect(none.bytes == plain.bytes)
    }

    @Test func withoutTheModelTheSubjectIsNearAndTheRestFallsOff() {
        let m = DepthComposer.compose(depth: nil, matte: leftQuarterMatte(16, 8), width: 16, height: 8)
        let at = { (x: Int, y: Int) in m.bytes[y * 16 + x] }
        #expect(at(1, 4) == UInt8((DepthComposer.fallbackSubject * 255).rounded()), "the subject is near")
        #expect(at(5, 4) > at(15, 4), "farther from the subject is farther away")
        #expect(at(1, 4) > at(15, 4))
        // No matte: only the ground gradient, nearer toward the bottom.
        let g = DepthComposer.compose(depth: nil, matte: nil, width: 4, height: 8)
        #expect(g.bytes[7 * 4] > g.bytes[0] && g.bytes[0] == g.bytes[3])
        // Degenerate sizes still give a pixel.
        #expect(DepthComposer.compose(depth: nil, matte: nil, width: 0, height: 0).bytes.count == 1)
    }

    @Test func distancesAreChamferDistancesToTheMatte() {
        let m: [Float] = [1, 0, 0, 0, 0, 0, 0, 0, 0]
        let d = DepthComposer.distances(m, 3, 3)
        #expect(d[0] == 0 && d[1] == 1 && d[2] == 2 && abs(d[4] - 1.4142) < 1e-4)
        #expect(DepthComposer.distances([0, 0], 2, 1).allSatisfy { $0 > 1e6 }, "no matte: far everywhere")
        #expect(DepthComposer.bilinear([1, 3], 2, 1, 0.5, 0.5) == 2)
        #expect(DepthComposer.bilinear([], 0, 0, 0.5, 0.5) == 0)
        #expect(DepthField(width: 2, height: 1, values: [0, 1]).sample(0, 0) == 0)
    }

    @Test func mapsAreAtMost1024OnTheLongEdge() {
        #expect(DepthMapService.mapSize(2048, 1024) == (1024, 512))
        #expect(DepthMapService.mapSize(800, 600) == (800, 600))
        #expect(DepthMapService.mapSize(0, 0) == (1, 1))
    }

    // MARK: Cache

    @Test func cacheFilesRoundTripAndRejectDamage() {
        let r = DepthResult(depth: VisionMask(width: 2, height: 1, bytes: [3, 250]), subject: leftQuarterMatte(), method: "m")
        let data = DepthMapService.encode(r)
        #expect(DepthMapService.decode(data, method: "m") == r)
        let bare = DepthResult(depth: VisionMask(width: 1, height: 1, bytes: [9]), subject: nil, method: "x")
        #expect(DepthMapService.decode(DepthMapService.encode(bare), method: "x") == bare)
        #expect(DepthMapService.decode(data.dropLast(), method: "m") == nil, "truncated")
        #expect(DepthMapService.decode(Data("NOPE0000000000".utf8), method: "m") == nil)
        #expect(DepthMapService.decode(data + Data([0]), method: "m") == nil, "trailing bytes")
        #expect(DepthMapService.read(URL(fileURLWithPath: "/nonexistent/x.depth"), method: "m") == nil)
    }

    /// Low: the disk cache keeps the most recently used maps within its size limit.
    @Test func theDiskCacheDropsTheLeastRecentlyUsedMaps() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthlru-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = DepthResult(depth: VisionMask(width: 10, height: 10, bytes: Array(repeating: 1, count: 100)), subject: nil, method: "m")
        let data = DepthMapService.encode(r)
        let now = Date()
        for (i, name) in ["a", "b", "c"].enumerated() {
            let f = dir.appendingPathComponent(name + ".depth")
            try data.write(to: f)
            try FileManager.default.setAttributes([.modificationDate: now.addingTimeInterval(Double(i - 10))], ofItemAtPath: f.path)
        }
        try Data([1]).write(to: dir.appendingPathComponent("other.txt"))
        // Reading "a" makes it the most recent.
        #expect(DepthMapService.read(dir.appendingPathComponent("a.depth"), method: "m") == r)
        DepthMapService.prune(dir, cap: Int64(data.count * 2))
        let left = Set(try FileManager.default.contentsOfDirectory(atPath: dir.path))
        #expect(left == ["a.depth", "c.depth", "other.txt"])
        DepthMapService.prune(dir, cap: Int64(data.count * 2)) // fits: nothing more
        DepthMapService.prune(dir.appendingPathComponent("missing"), cap: 0)
        #expect(try FileManager.default.contentsOfDirectory(atPath: dir.path).count == 3)
        #expect(DepthMapService.diskCap >= 256 << 20)
    }

    @Test func theKeyFollowsTheFileAndTheMethod() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthkey-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let f = dir.appendingPathComponent("a.ARW")
        try Data([1, 2, 3]).write(to: f)
        let k = try #require(DepthMapService.key(for: f, method: "m1"))
        #expect(k.count == 64 && DepthMapService.key(for: f, method: "m1") == k)
        #expect(DepthMapService.key(for: f, method: "m2") != k, "another model makes another map")
        try Data([1, 2, 3, 4]).write(to: f) // rewritten: a new photo
        #expect(DepthMapService.key(for: f, method: "m1") != k)
        #expect(DepthMapService.key(for: nil, method: "m1") == nil)
        #expect(DepthMapService.key(for: dir.appendingPathComponent("missing"), method: "m1") == nil)
        #expect(DepthMapService.modelID(.depthAnything) == "DepthAnythingV2SmallF16-cfef6f6f2a70")
    }

    // MARK: Service

    @MainActor
    func service(_ dir: URL, estimator: (any DepthEstimator)? = HalfDepth(), matte: VisionMask? = leftQuarterMatte())
        -> DepthMapService {
        let model = DownloadableModel(root: dir.appendingPathComponent("Models"), descriptor: .depthAnything)
        let est = estimator
        return DepthMapService(model: model, cacheDirectory: dir.appendingPathComponent("Cache/Depth"),
                               matte: { _ in matte }, estimator: { est })
    }

    @MainActor
    func photoFile(_ dir: URL) throws -> Photo {
        let f = dir.appendingPathComponent("p.ARW")
        try Data([7]).write(to: f)
        var p = samplePhoto()
        p.url = f
        return p
    }

    @MainActor
    @Test func mapsAreComputedInTheBackgroundCachedAndReloadedFromDisk() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthsvc-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let photo = try photoFile(dir)
        let s = service(dir)
        var ready: [PhotoID] = []
        s.onDepthReady = { ready.append($0) }
        // Not waited for: nil now, the map lands later.
        #expect(await s.depth(for: photo, wait: false) { grayImage() } == nil)
        #expect(await waitUntil { ready == [photo.id] })
        let hit = try #require(await s.depth(for: photo, wait: false) { nil })
        #expect(hit.method == "fake-depth-1" && hit.subject == leftQuarterMatte())
        #expect(hit.depth.width == 64 && hit.depth.height == 32)
        #expect(hit.depth.bytes[0] == hit.depth.bytes[1], "the subject is flattened")
        #expect(s.computed == 1)
        // A new service (a relaunch) reads it from disk without computing.
        let s2 = service(dir)
        let disk = try #require(await s2.depth(for: photo, wait: false) { nil })
        #expect(disk.depth == hit.depth && s2.computed == 0)
        // The fallback is another map: computed anew, waited for.
        let s3 = service(dir, estimator: nil)
        let fb = try #require(await s3.depth(for: photo, wait: true) { grayImage() })
        #expect(fb.method == DepthMapService.fallbackMethod && fb.depth != hit.depth)
        // No image: nothing, and nothing cached.
        let s4 = service(dir.appendingPathComponent("other"))
        #expect(await s4.depth(for: photo, wait: true) { nil } == nil)
        // A photo without a file has no map.
        #expect(await s4.depth(for: samplePhoto(), wait: true) { grayImage() } == nil)
        // The memory cache keeps the most recent maps.
        #expect(DepthMapService.memoryLimit >= 4)
    }

    @MainActor
    @Test func cancellingStopsTheWork() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthcancel-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let photo = try photoFile(dir)
        let s = service(dir)
        let gate = AsyncStream<Void>.makeStream()
        let waiter = Task { @MainActor in
            await s.depth(for: photo, wait: true) {
                for await _ in gate.stream { break }
                return grayImage()
            }
        }
        try await Task.sleep(for: .milliseconds(20))
        waiter.cancel()
        gate.continuation.yield()
        #expect(await waiter.value == nil, "a cancelled export gets no map")
        await s.settle()
        #expect(s.computed == 0)
        // cancel(_:) stops a background computation too.
        let gate2 = AsyncStream<Void>.makeStream()
        _ = await s.depth(for: photo, wait: false) {
            for await _ in gate2.stream { break }
            return grayImage()
        }
        s.cancel(photo)
        gate2.continuation.yield()
        await s.settle()
        #expect(s.computed == 0)
    }

    /// Med 4: cancelling one export waiting for a map doesn't stop it for the others (a
    /// second waiter, or the viewer that asked in the background).
    @MainActor
    @Test func cancellingOneWaiterKeepsASharedJob() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthshare-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let photo = try photoFile(dir)
        let s = service(dir)
        var ready: [PhotoID] = []
        s.onDepthReady = { ready.append($0) }
        let gate = AsyncStream<Void>.makeStream()
        let slow: @Sendable () async -> CGImage? = {
            for await _ in gate.stream { break }
            return grayImage()
        }
        let a = Task { @MainActor in await s.depth(for: photo, wait: true, image: slow) }
        let b = Task { @MainActor in await s.depth(for: photo, wait: true, image: slow) }
        #expect(await waitUntil { s.waiters(for: photo.id) == 2 })
        a.cancel()
        try await Task.sleep(for: .milliseconds(20))
        gate.continuation.yield()
        #expect(await b.value != nil, "the other waiter still gets its map")
        _ = await a.value
        #expect(await waitUntil { s.computed == 1 })
        // In the background: a waiter giving up doesn't stop what the viewer asked for.
        let other = dir.appendingPathComponent("q.ARW")
        try Data([8]).write(to: other)
        var p2 = samplePhoto(2)
        p2.url = other
        let gate2 = AsyncStream<Void>.makeStream()
        let slow2: @Sendable () async -> CGImage? = {
            for await _ in gate2.stream { break }
            return grayImage()
        }
        #expect(await s.depth(for: p2, wait: false, image: slow2) == nil)
        let c = Task { @MainActor in await s.depth(for: p2, wait: true, image: slow2) }
        #expect(await waitUntil { s.waiters(for: p2.id) == 1 })
        c.cancel()
        gate2.continuation.yield()
        await s.settle()
        #expect(await waitUntil { s.computed == 2 }, "finished for the viewer")
        #expect(await waitUntil { ready == [photo.id, p2.id] }, "and the viewer hears it landed")
    }

    /// Review 2: giving up and joining can't interleave: a waiter joining a job that the
    /// last other waiter just stopped gets a fresh job (and its map), never nil.
    @MainActor
    @Test func aJoinerNeverGetsAStoppedJob() async throws {
        let parked = Task<DepthResult?, Never> { try? await Task.sleep(for: .seconds(30)); return nil }
        let i = JobInterest(parked)
        #expect(i.join(waiting: true))
        i.waiterLeft() // the only waiter gave up: stopped, atomically
        #expect(i.isStopped && parked.isCancelled)
        #expect(!i.join(waiting: true) && !i.join(waiting: false), "a stopped job takes nobody")
        let kept = Task<DepthResult?, Never> { try? await Task.sleep(for: .seconds(30)); return nil }
        let k = JobInterest(kept)
        #expect(k.join(waiting: false) && k.join(waiting: true))
        k.waiterLeft()
        #expect(!k.isStopped && !kept.isCancelled, "the background still wants it")
        k.stop()
        #expect(kept.isCancelled)
        // Through the service: A gives up, B asks right after and still gets its map.
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthjoin-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let photo = try photoFile(dir)
        let s = service(dir)
        let gate = AsyncStream<Void>.makeStream()
        let a = Task { @MainActor in
            await s.depth(for: photo, wait: true) {
                for await _ in gate.stream { break }
                return grayImage()
            }
        }
        #expect(await waitUntil { s.waiters(for: photo.id) == 1 })
        a.cancel()
        let b = await s.depth(for: photo, wait: true) { grayImage() }
        #expect(b != nil, "B got a map from a fresh job")
        gate.continuation.yield()
        #expect(await a.value == nil)
    }

    /// Review 1: a free slot goes to an urgent job before the grid's queued ones, and a
    /// job stopped while queued gives its place up without ever asking for its image.
    @Test func urgentJobsGoFirstAndStoppedOnesNeverStart() async {
        let slots = JobSlots(limit: 1)
        #expect(await slots.acquire(JobInterest()))
        let order = DepthOrder()
        let grid = (0..<3).map { _ in JobInterest() }
        for (n, i) in grid.enumerated() {
            #expect(i.join(waiting: false))
            Task.detached {
                let ok = await slots.acquire(i)
                order.add("grid\(n):\(ok)")
                if ok { await slots.release() }
            }
            while await slots.queued < n + 1 { await Task.yield() } // queued in this order
        }
        let export = JobInterest()
        #expect(export.join(waiting: true) && export.isUrgent)
        let urgent = Task.detached {
            let ok = await slots.acquire(export)
            order.add("export:\(ok)")
            if ok { await slots.release() }
        }
        while await slots.queued < 4 { await Task.yield() }
        #expect(grid[1].stopIfOnlyBackground() && !export.stopIfOnlyBackground())
        await slots.release()
        await urgent.value
        while order.items.count < 4 { await Task.yield() }
        #expect(order.items.filter { $0.hasSuffix(":true") } == ["export:true", "grid0:true", "grid2:true"],
                "the export first, then the grid in order: \(order.items)")
        #expect(order.items.contains("grid1:false"), "the stopped one never ran")
        let stopped = JobInterest()
        stopped.stop()
        #expect(await !slots.acquire(stopped), "a stopped job never takes a slot")
    }

    /// Review 1 through the service: with the slots busy on the grid's maps, a waited-for
    /// map is made next; a photo that leaves the screen stops its background map, which
    /// never asks for its image.
    @MainActor
    @Test func anExportsMapIsNotStarvedByTheGrid() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthlanes-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = service(dir)
        let asked = DepthOrder()
        let gate = AsyncStream<Void>.makeStream()
        func photo(_ n: Int) throws -> Photo {
            let f = dir.appendingPathComponent("p\(n).ARW")
            try Data([UInt8(n)]).write(to: f)
            var p = samplePhoto(Int64(n + 1))
            p.url = f
            return p
        }
        func image(_ n: Int) -> @Sendable () async -> CGImage? {
            { asked.add("p\(n)")
              if n < 2 { for await _ in gate.stream { break } }
              return grayImage() }
        }
        var photos: [Photo] = []
        for n in 0..<6 {
            let p = try photo(n)
            photos.append(p)
            if n < 5 { _ = await s.depth(for: p, wait: false, image: image(n)) } // the grid
        }
        #expect(await waitUntil { asked.items.count == 2 }, "two slots, both on the grid")
        s.leftScreen(photos[3].id) // scrolled away
        let export = Task { @MainActor in await s.depth(for: photos[5], wait: true, image: image(5)) }
        #expect(await waitUntil { s.waiters(for: photos[5].id) == 1 })
        gate.continuation.yield() // one grid map finishes
        #expect(await export.value != nil)
        #expect(asked.items.prefix(3).last == "p5", "the export's map next, not after the grid: \(asked.items)")
        gate.continuation.yield()
        await s.settle()
        #expect(!asked.items.contains("p3"), "the stopped map never asked for its image")
        // The photo on screen and waited maps aren't stopped by leaving the screen.
        let i = JobInterest()
        #expect(i.join(waiting: false, urgent: true) && !i.stopIfOnlyBackground())
    }

    /// Review 1: an export waiting for a map stops when the export is cancelled, and a
    /// map that takes too long fails the photo (nil) instead of hanging the export.
    @Test func anExportsWaitEndsOnCancelOrTimeout() async {
        let map = DepthResult(depth: VisionMask(width: 1, height: 1, bytes: [1]), subject: nil, method: "m")
        let never: DepthProvider = { _, _ in
            try? await Task.sleep(for: .seconds(60))
            return Task.isCancelled ? nil : map
        }
        let slow = ExportDepth(photos: [1: PhotoID(1)], timeout: .milliseconds(200), provider: never)
        let cancelled = ExportDepth(photos: [1: PhotoID(1)], provider: never)
        // Not about timing: a timeout a contended machine can't hit.
        let fast = ExportDepth(photos: [1: PhotoID(1)], timeout: .seconds(60)) { _, _ in map }
        let bound = 5 * TestTiming.scale
        let got = await withCheckedContinuation { (c: CheckedContinuation<[Bool], Never>) in
            Thread.detachNewThread {
                let t0 = Date()
                let a = slow.map(1) == nil && Date().timeIntervalSince(t0) < bound
                // Its own thread: the suite parks GCD's global threads in hung fakes.
                Thread.detachNewThread { Thread.sleep(forTimeInterval: 0.1); cancelled.cancel() }
                let t1 = Date()
                let b = cancelled.map(1) == nil && Date().timeIntervalSince(t1) < bound
                c.resume(returning: [a, b, fast.map(1) == map])
            }
        }
        #expect(got == [true, true, true])
        // Low 3: each says why: too slow fails the photo saying so, a cancelled wait isn't
        // a failure (the export was cancelled), a map that couldn't be made says that.
        #expect(slow.gaveUp(1) == .tooSlow && cancelled.gaveUp(1) == .cancelled && fast.gaveUp(1) == nil)
        let made = "not exported: its lens blur's depth map couldn't be made on this Mac"
        #expect(ExportDepth.failureMessage(made, gaveUp: .tooSlow) == "not exported: its lens blur's depth map took too long")
        #expect(ExportDepth.failureMessage(made, gaveUp: .cancelled) == nil)
        #expect(ExportDepth.failureMessage(made, gaveUp: nil) == made)
    }

    /// Med 4: a depth model that fails on a photo leaves the fallback's map for this run,
    /// not on disk under the model's name for good.
    @MainActor
    @Test func aFailingModelIsNotCachedAsItsMap() async throws {
        struct Broken: DepthEstimator {
            var id = "broken-1"
            func estimate(_ image: CGImage) throws -> DepthField { throw SegmentationError.download("no") }
        }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthfail-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let photo = try photoFile(dir)
        let s = service(dir, estimator: Broken())
        let got = try #require(await s.depth(for: photo, wait: true) { grayImage() })
        #expect(got.method == DepthMapService.fallbackMethod)
        #expect(await waitUntil { s.computed == 1 })
        #expect(await s.depth(for: photo, wait: false) { nil } == got, "kept in memory: no retry loop")
        let files = (try? FileManager.default.contentsOfDirectory(atPath: dir.appendingPathComponent("Cache/Depth").path)) ?? []
        #expect(files.isEmpty, "nothing on disk")
        let s2 = service(dir, estimator: Broken())
        _ = await s2.depth(for: photo, wait: true) { grayImage() }
        #expect(await waitUntil { s2.computed == 1 }, "the next launch tries the model again")
    }

    /// Med 4: at most `concurrentJobs` maps are made at once.
    @MainActor
    @Test func mapsAreMadeAFewAtATime() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthbound-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = service(dir)
        let entered = DepthCounter()
        let gate = AsyncStream<Void>.makeStream()
        let blocked: @Sendable () async -> CGImage? = {
            entered.add()
            for await _ in gate.stream { break }
            return grayImage()
        }
        for n in 0..<4 {
            let f = dir.appendingPathComponent("p\(n).ARW")
            try Data([UInt8(n)]).write(to: f)
            var p = samplePhoto(Int64(n + 1))
            p.url = f
            _ = await s.depth(for: p, wait: false, image: blocked)
        }
        #expect(await waitUntil { entered.value == DepthMapService.concurrentJobs })
        try await Task.sleep(for: .milliseconds(100))
        #expect(entered.value == DepthMapService.concurrentJobs, "the others wait for a slot")
        for _ in 0..<4 { gate.continuation.yield() }
        await s.settle()
        #expect(await waitUntil { s.computed == 4 } && entered.value == 4)
        let slots = JobSlots(limit: 1)
        #expect(await slots.acquire(JobInterest()))
        await slots.release()
        #expect(await slots.busy == 0)
    }

    @MainActor
    @Test func statusAndDownloadFollowTheModel() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthdl-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = service(dir)
        #expect(s.status.contains("50 MB"))
        // A failing download reports why and leaves the subject-only depth.
        let failing = DepthMapService(model: DownloadableModel(root: dir, descriptor: .depthAnything, downloader: FailingDownloader()),
                                      cacheDirectory: dir)
        var called = false
        failing.download { called = true }
        #expect(await waitUntil { if case .failed = failing.model.state { return true }; return false })
        #expect(!called && failing.status.contains("depth model"))
        // Without the model the default estimator is the fallback.
        #expect(await failing.estimator() == nil)
    }

    // MARK: Wiring

    @MainActor
    @Test func rendersGetTheDepthMapAndItsSubjectForAutoFocus() async {
        let map = DepthResult(depth: VisionMask(width: 2, height: 1, bytes: [0, 255]), subject: leftQuarterMatte(), method: "m")
        let provider: DepthProvider = { _, _ in map }
        var e = EditState()
        #expect(await StaticMasks.gather(e, photo: PhotoID(1), provider: nil, persons: nil, depth: provider) == nil,
                "no lens blur: no depth asked for")
        e.effects.lensBlur.amount = 40
        let got = await StaticMasks.gather(e, photo: PhotoID(1), provider: nil, persons: nil, depth: provider)
        #expect(got?.depthMap()?.data == Data([0, 255]))
        #expect(got?.aiMask(target: .subject)?.width == 8, "the matte serves the auto focus")
        e.effects.lensBlur.mode = .plane
        #expect(await StaticMasks.gather(e, photo: PhotoID(1), provider: nil, persons: nil, depth: provider) == nil,
                "tilt-shift needs no depth")
        // Export: per source id.
        let x = ExportMasks([:], depth: ExportDepth([3: map]))
        #expect(x.depthMap(sourceId: 3)?.width == 2 && x.depthMap(sourceId: 4) == nil)
        #expect(x.aiMask(sourceId: 3, target: .subject)?.width == 8, "the matte serves the auto focus")
        #expect(x.aiMask(sourceId: 3, target: .sky) == nil && x.aiMask(sourceId: 4, target: .subject) == nil)
    }

    /// Med 7: an export makes each photo's map when its render asks (not all up front),
    /// keeps only the last few, and asks once per photo while it is kept.
    @Test func exportsMakeMapsPerPhotoAndKeepAFew() async {
        let asked = DepthCounter()
        let map = DepthResult(depth: VisionMask(width: 2, height: 1, bytes: [0, 255]), subject: nil, method: "m")
        let photos = Dictionary(uniqueKeysWithValues: (0..<10).map { (UInt64($0), PhotoID(Int64($0) + 1)) })
        let d = ExportDepth(photos: photos) { _, wait in
            #expect(wait, "an export waits for its maps")
            asked.add()
            return map
        }
        #expect(asked.value == 0, "nothing made up front")
        // Off the Swift concurrency pool, like the export's own thread.
        let got = await withCheckedContinuation { (c: CheckedContinuation<[DepthResult?], Never>) in
            Thread.detachNewThread {
                var out: [DepthResult?] = []
                for id in 0..<10 { out.append(d.map(UInt64(id))) }
                out.append(d.map(9)) // kept: not made again
                out.append(d.map(0)) // long gone: made again
                out.append(d.map(42)) // no lens blur
                c.resume(returning: out)
            }
        }
        #expect(got.prefix(11).allSatisfy { $0 == map } && got[11] == map && got[12] == nil)
        #expect(asked.value == 11)
        let m = ExportMasks([:], depth: d)
        #expect(m.depthMap(sourceId: 9)?.data == Data([0, 255]) && m.aiMask(sourceId: 9, target: .subject) == nil)
        // The export's gather asks for no map itself.
        var svc = RustExportService()
        svc.depthProvider = { _, _ in
            asked.add()
            return map
        }
        var blur = EditState()
        blur.effects.lensBlur.amount = 40
        let item = { (edit: EditState?) in ExportJobItem(photoID: PhotoID(1), url: URL(fileURLWithPath: "/tmp/a"), seq: 1, edit: edit) }
        let before = asked.value
        #expect(await svc.gatherMasks([1: item(blur), 2: item(nil)]).masks != nil && asked.value == before)
        #expect(await svc.gatherMasks([2: item(EditState())]).masks == nil, "no depth blur: no masks")
    }

    @MainActor
    @Test func theAppRendersAgainWhenTheFocusedPhotosMapLands() async throws {
        let m = makeModel()
        await m.start()
        let id = try #require(m.focusedID)
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("depthapp-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = service(dir)
        m.wireDepth(service: s)
        defer { DepthWiring.provider = nil }
        #expect(DepthWiring.provider != nil)
        // Mock photos have no file: no map.
        #expect(await DepthWiring.provider?(id, true) == nil)
        #expect(await m.depthForRender(PhotoID(999_999), wait: true, service: s) == nil)
        let before = m.viewer.displayVersion
        m.depthReady(id) // no lens blur: nothing to redo
        m.updateEdits([id], label: "blur") { _, e in e.effects.lensBlur.amount = 50 }
        m.depthReady(id)
        m.depthReady(PhotoID(999_999))
        _ = before
        s.onDepthReady(id)
        m.downloadDepthModel(service: s)
    }

    /// H1: a depth lens blur thumbnail made before the photo's map landed is made again
    /// when it lands (any photo, not only the one on screen); other edits keep theirs.
    @MainActor
    @Test func aLandedMapRefreshesThatPhotosLensBlurThumbnail() async throws {
        let m = makeModel()
        await m.start()
        let id = try #require(m.focusedID)
        let other = try #require(m.photos.first { $0.id != id }?.id)
        m.thumbnails.tools = FakeDevelopTools()
        let photo = try #require(m.photo(id)), otherPhoto = try #require(m.photo(other))
        m.updateEdits([id, other], label: "edit") { _, e in e.exposure.ev = 1 }
        let plain = m.thumbnails.fingerprint(photo)
        m.depthReady(id)
        #expect(m.thumbnails.fingerprint(photo) == plain, "no lens blur: no depth in its identity")
        m.updateEdits([id, other], label: "blur") { _, e in e.effects.lensBlur.amount = 50 }
        let blurred = m.thumbnails.fingerprint(photo), otherBlurred = m.thumbnails.fingerprint(otherPhoto)
        m.focus(id, extend: false)
        m.depthReady(other) // not the photo on screen
        #expect(m.thumbnails.fingerprint(otherPhoto) != otherBlurred, "its thumbnail renders again")
        #expect(m.thumbnails.fingerprint(photo) == blurred)
        m.depthReady(id)
        #expect(m.thumbnails.fingerprint(photo) != blurred)
        #expect(ThumbnailCache(provider: MockImageProvider(latency: .zero)).depthVersion(id) == 0)
    }

    @Test func coreMLOutputBuffersReadAsFields() throws {
        func buffer(_ format: OSType, _ fill: (UnsafeMutableRawPointer, Int) -> Void) -> CVPixelBuffer {
            var pb: CVPixelBuffer?
            CVPixelBufferCreate(nil, 3, 2, format, nil, &pb)
            CVPixelBufferLockBaseAddress(pb!, [])
            fill(CVPixelBufferGetBaseAddress(pb!)!, CVPixelBufferGetBytesPerRow(pb!))
            CVPixelBufferUnlockBaseAddress(pb!, [])
            return pb!
        }
        let half = buffer(kCVPixelFormatType_OneComponent16Half) { p, row in
            for y in 0..<2 { for x in 0..<3 { (p + y * row).assumingMemoryBound(to: UInt16.self)[x] = Float16(Float(x) / 2).bitPattern } }
        }
        #expect(CoreMLDepthEstimator.field(half)?.values == [0, 0.5, 1, 0, 0.5, 1])
        let float = buffer(kCVPixelFormatType_OneComponent32Float) { p, row in
            for y in 0..<2 { for x in 0..<3 { (p + y * row).assumingMemoryBound(to: Float.self)[x] = Float(y) } }
        }
        #expect(CoreMLDepthEstimator.field(float)?.values == [0, 0, 0, 1, 1, 1])
        let byte = buffer(kCVPixelFormatType_OneComponent8) { p, row in
            for y in 0..<2 { for x in 0..<3 { (p + y * row).assumingMemoryBound(to: UInt8.self)[x] = 255 } }
        }
        #expect(CoreMLDepthEstimator.field(byte)?.values == Array(repeating: 1, count: 6))
        let bgra = buffer(kCVPixelFormatType_32BGRA) { _, _ in }
        #expect(CoreMLDepthEstimator.field(bgra) == nil)
    }

    /// The real model, when a local copy is given (`DS_DEPTH_MODEL_DIR`): a frame lit and
    /// near at the bottom gives a depth map, and the left matte is one plane.
    @Test func theRealModelMakesADepthMap() async throws {
        guard let pkg = ProcessInfo.processInfo.environment["DS_DEPTH_MODEL_DIR"],
              FileManager.default.fileExists(atPath: pkg) else { return }
        let compiled = try await MLModel.compileModel(at: URL(fileURLWithPath: pkg))
        defer { try? FileManager.default.removeItem(at: compiled) }
        let est = CoreMLDepthEstimator(model: try MLModel(contentsOf: compiled), id: "real")
        let field = try est.estimate(grayImage(518, 392))
        #expect(field.width == 518 && field.height == 392 && field.values.allSatisfy(\.isFinite))
        let map = DepthComposer.compose(depth: field, matte: leftQuarterMatte(), width: 518, height: 392)
        #expect(map.bytes[0] == map.bytes[50], "inside the matte: one plane")
    }
}

/// Things in the order they happened, from any thread (tests).
final class DepthOrder: @unchecked Sendable {
    private let lock = NSLock()
    private var list: [String] = []
    var items: [String] { lock.withLock { list } }
    func add(_ s: String) { lock.withLock { list.append(s) } }
}

/// A thread-safe count (tests).
final class DepthCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    var value: Int { lock.withLock { n } }
    func add() { lock.withLock { n += 1 } }
}

/// Every download fails.
struct FailingDownloader: ModelDownloader {
    func download(_ url: URL, to dst: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
        throw SegmentationError.download("offline")
    }
}
