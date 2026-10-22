import CoreGraphics
import DarkslideFFI
import Foundation
import ImageIO
import Metal
import Testing
@testable import DarkslideKit

private func sum(_ f: RenderedFrame) -> Int {
    f.withBytes { p in
        var s = 0
        for y in 0..<f.height {
            for x in 0..<(f.width * 4) { s += Int(p[y * f.bytesPerRow + x]) }
        }
        return s
    }
}

private func samplesDir() -> URL? {
    let root = URL(fileURLWithPath: #filePath).resolvingSymlinksInPath().deletingLastPathComponent().deletingLastPathComponent()
        .deletingLastPathComponent().deletingLastPathComponent()
    var candidates = [root.appendingPathComponent("samples"),
                      root.appendingPathComponent("../../../samples").standardizedFileURL]
    if let env = ProcessInfo.processInfo.environment["DS_SAMPLES_DIR"] {
        candidates.insert(URL(fileURLWithPath: env), at: 0)
    }
    return candidates.first { FileManager.default.fileExists(atPath: $0.path) }
}

private func sampleARW() -> URL? {
    guard let dir = samplesDir(),
          let files = try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil) else { return nil }
    return files.filter { $0.pathExtension.uppercased() == "ARW" }.sorted { $0.path < $1.path }.first
}

@Suite struct RustRendererTests {
    @Test func requestAndGeometryMapping() {
        #expect(RustRenderer.sourceSize(samplePhoto()) == (6000, 4000))
        var p = samplePhoto()
        p.meta.pixelWidth = 0
        p.meta.pixelHeight = 999_999
        #expect(RustRenderer.sourceSize(p) == (1, 65_536))

        let r = RustRenderer.ffiRequest(RenderRequest(maxPixelSize: 0, scale: 1, region: CGRect(x: 0.5, y: 0.25, width: 0.1, height: 0.2),
                                                      format: .rgba16fLinearP3))
        #expect(r.maxLongEdge == 1 && r.scale == 1 && r.format == .rgba16fLinearP3)
        #expect(r.region == NormRect(x: 0.5, y: 0.25, w: 0.1, h: 0.2))
        #expect(RustRenderer.ffiRequest(RenderRequest(maxPixelSize: 900)).region == nil)

        let g = RustRenderer.frameGeometry(FrameInfo(
            width: 10, height: 5, bytesPerRow: 256, format: .rgba8Srgb, fullWidth: 100, fullHeight: 50,
            originX: 50, originY: 25, region: NormRect(x: 0.5, y: 0.5, w: 0.1, h: 0.1)))
        #expect(g == RustRenderer.Geometry(width: 10, height: 5, bytesPerRow: 256, format: .rgba8Srgb,
                                           region: CGRect(x: 0.5, y: 0.5, width: CGFloat(Float(0.1)), height: CGFloat(Float(0.1))),
                                           fullSize: CGSize(width: 100, height: 50)))
    }

    @Test func paramsMapEditUnits() {
        var e = EditState()
        #expect(RustRenderer.params(for: e) == TestPatternParams(exposureEv: 0, warmth: 0, saturation: 0))
        e.exposure.ev = 9
        e.exposure.saturation = -50
        e.whiteBalance.asShot = false
        e.whiteBalance.temperature = 10_000
        let p = RustRenderer.params(for: e)
        #expect(p.exposureEv == 5 && p.saturation == -0.5 && p.warmth == 1)
        e.whiteBalance.temperature = 3250
        #expect(RustRenderer.params(for: e).warmth == -0.5)
    }

    @Test func rendersIntoAMetalBufferWithAlignedRows() async throws {
        let frame = try await RustRenderer().render(samplePhoto(portrait: true), edit: EditState(), maxPixelSize: 300)
        #expect(frame.width == 200 && frame.height == 300 && frame.format == .rgba8Srgb)
        #expect(frame.bytesPerRow == 1024, "rows padded to 256 bytes")
        guard case .buffer(let buf) = frame.backing else {
            Issue.record("expected an MTLBuffer backing")
            return
        }
        #expect(buf.storageMode == .shared && buf.length >= 1024 * 300)
        #expect(frame.makeCGImage()?.width == 200)
        #expect(frame.fullSize == CGSize(width: 200, height: 300) && frame.region == nil)
        var bright = EditState()
        bright.exposure.ev = 2
        let b = try await RustRenderer().render(samplePhoto(), edit: bright, maxPixelSize: 64)
        let d = try await RustRenderer().render(samplePhoto(), edit: EditState(), maxPixelSize: 64)
        #expect(sum(b) > sum(d))
    }

    @Test func heapFallbackWithoutMetal() async throws {
        let frame = try await RustRenderer(device: nil).render(samplePhoto(), edit: EditState(), maxPixelSize: 900)
        guard case .data = frame.backing else {
            Issue.record("expected Data backing")
            return
        }
        let white = SelfTest.pixel(frame, x: Int(0.5 / 6 * 900), y: Int((0.45 + 3.5 / 4 * 0.40) * 600))
        #expect(white == [243, 243, 242, 255])
    }

    @Test func oneToOneTileCoversExactlyTheRegion() async throws {
        let req = RenderRequest(maxPixelSize: 1, scale: 1, region: CGRect(x: 0.5, y: 0.5, width: 0.01, height: 0.01))
        let tile = try await RustRenderer().render(samplePhoto(), edit: EditState(), request: req)
        #expect(tile.width == 60 && tile.height == 40)
        #expect(tile.fullSize == CGSize(width: 6000, height: 4000))
        #expect(abs((tile.region?.minX ?? 0) - 0.5) < 1e-6)
        let half = try await RustRenderer().render(samplePhoto(), edit: EditState(),
                                                   request: RenderRequest(maxPixelSize: 1, scale: 0.5, region: req.region))
        #expect(half.width == 30 && half.height == 20 && half.fullSize == CGSize(width: 3000, height: 2000))
    }

    @Test func halfFloatFramesAreLinearP3() async throws {
        var p = samplePhoto()
        (p.meta.pixelWidth, p.meta.pixelHeight) = (1100, 100)
        var up = EditState()
        up.exposure.ev = 1
        let f = try await RustRenderer().render(p, edit: up, request: RenderRequest(maxPixelSize: 1100, format: .rgba16fLinearP3))
        #expect(f.format == .rgba16fLinearP3 && f.bytesPerRow >= 1100 * 8)
        let white = SelfTest.halfPixel(f, x: 1095, y: 98)
        #expect(abs(white[0] - 2) < 0.01 && abs(white[1] - 2) < 0.01, "EDR: +1 EV white = 2.0, got \(white)")
        #expect(f.makeCGImage() == nil, "no 8-bit CGImage view of a float frame")
        #expect(Histogram(frame: f).pixelCount == 0)
    }

    @Test func invalidRequestsThrowRenderError() async {
        await #expect(throws: RenderError.self) {
            try await RustRenderer().render(samplePhoto(), edit: EditState(), request: RenderRequest(maxPixelSize: 1, scale: 100))
        }
        var huge = samplePhoto()
        huge.meta.pixelWidth = 30_000
        await #expect(throws: RenderError.self, "a 30000 px wide 1:1 frame needs a region (tile)") {
            try await RustRenderer().render(huge, edit: EditState(), request: RenderRequest(maxPixelSize: 1, scale: 1))
        }
    }

    @Test func cancellingTheTaskStopsRustEarly() async throws {
        let renderer = RustRenderer(device: nil)
        let photo = samplePhoto()
        let request = RenderRequest(maxPixelSize: 6000, scale: 1)
        let clock = ContinuousClock()
        let t0 = clock.now
        _ = try await renderer.render(photo, edit: EditState(), request: request)
        let full = (clock.now - t0).inMilliseconds

        // Cancel 1 ms in. A loaded machine can wake this task only after the render is
        // done (nothing left to cancel): try again then, a few times.
        var cancelled: (error: any Error, after: Double)?
        for _ in 0..<10 {
            let task = Task { try await renderer.render(photo, edit: EditState(), request: request) }
            try await Task.sleep(for: .milliseconds(1))
            let t1 = clock.now
            task.cancel()
            let result = await task.result
            if case .failure(let e) = result {
                cancelled = (e, (clock.now - t1).inMilliseconds)
                break
            }
        }
        guard let cancelled else {
            // The machine finished the whole render within 1 ms of starting: nothing to cancel.
            #expect(full < 5)
            return
        }
        #expect(cancelled.error is CancellationError)
        #expect(cancelled.after < max(full, 20), "returned \(cancelled.after) ms after cancel; a full render takes \(full) ms")

        let pre = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await renderer.render(photo, edit: EditState(), maxPixelSize: 64)
        }
        await #expect(throws: CancellationError.self) { try await pre.value }
    }

    @Test func cancelledRenderWritesOnlyAFewBands() async throws {
        // Drive the FFI directly with our own buffer to see how much Rust wrote.
        let engine = try Engine()
        let req = DarkslideFFI.RenderRequest(maxLongEdge: 1, scale: 1, region: nil, format: .rgba8Srgb)
        let info = try planFrame(sourceWidth: 4000, sourceHeight: 16_000, request: req)
        let len = Int(info.bytesPerRow) * Int(info.height)
        let buf = UnsafeMutableRawPointer.allocate(byteCount: len, alignment: 256)
        defer { buf.deallocate() }
        memset(buf, 0, len)
        let token = try CancelToken()
        let addr = UInt64(UInt(bitPattern: buf))
        let clock = ContinuousClock()
        let started = clock.now
        let task = Task {
            try await engine.renderTestPatternInto(
                sourceWidth: 4000, sourceHeight: 16_000, params: TestPatternParams(exposureEv: 0, warmth: 0, saturation: 0),
                request: req, destPtr: addr, destLen: UInt64(len), token: token)
        }
        try await Task.sleep(for: .milliseconds(3))
        try? token.cancel()
        let cancelledAt = clock.now
        let result = await task.result
        let latency = (clock.now - cancelledAt).inMilliseconds
        let bands = Int(info.height) / 64
        var written = 0
        for b in 0..<bands where buf.load(fromByteOffset: b * 64 * Int(info.bytesPerRow) + 3, as: UInt8.self) == 255 {
            written += 1
        }
        if case .success = result {
            Issue.record("render finished in \((cancelledAt - started).inMilliseconds) ms before the cancel")
            return
        }
        #expect(written < bands / 2, "\(written)/\(bands) bands written before stopping")
        // Early stop is proven by the band count above. Wall-clock latency is only sanity-checked
        // here (the suite shares the rayon pool and the machine); the tight bound (< 50 ms) is
        // enforced standalone by `Darkslide --self-test` and ZFFI.cancelLatency1to1.
        #expect(latency < 5000, "stopped \(latency) ms after cancel")
    }

    @Test func progressStreamDeliversInOrder() async throws {
        let engine = try Engine()
        let stream = ProgressStream(bufferingNewest: 256)
        try engine.setProgressListener(listener: stream)
        _ = try await RustRenderer(engine: engine).render(samplePhoto(), edit: EditState(), maxPixelSize: 300)
        #expect(try engine.flushProgress())
        try engine.setProgressListener(listener: nil)
        stream.finish()
        var phases: [TaskPhase] = []
        for await e in stream.events { phases.append(e.phase) }
        #expect(phases.first == .started && phases.last == .finished)
        #expect(phases.dropFirst().dropLast().allSatisfy { $0 == .running } && phases.count >= 3)
        #expect(try engine.droppedProgressEvents() == 0)
    }

    @Test func progressStreamKeepsOnlyTheNewest() async {
        let stream = ProgressStream(bufferingNewest: 2)
        for i in 0..<10 {
            stream.onProgress(event: ProgressEvent(taskId: 1, kind: "t", phase: .running, completed: UInt64(i), total: 10))
        }
        stream.finish()
        var got: [UInt64] = []
        for await e in stream.events { got.append(e.completed) }
        #expect(got == [8, 9])
    }

    @Test func errorsMapToRenderError() {
        #expect(RenderError(.InvalidArgument(message: "bad")).kind == .invalidArgument)
        #expect(RenderError(.Cancelled).kind == .cancelled)
        #expect(RenderError(.Io(message: "disk")).errorDescription == "disk")
        #expect(RenderError(.Unsupported(message: "no")).kind == .unsupported)
        let bug = RenderError(.Internal(message: "worker panicked: bug"))
        #expect(bug.kind == .internalError && bug.errorDescription == "Internal error: worker panicked: bug")
    }

    @Test func rustAgreesOnContractAndGroups() {
        #expect(RustCore.contract == expectedFFIContract)
        #expect(RustCore.version.contains("ffi \(expectedFFIContract)"))
        #expect(RustCore.groups.map(\.0) == AdjustGroup.allCases)
        #expect(RustCore.groups.allSatisfy { $0.0.supportsRelative == $0.1 })
    }

    @MainActor
    @Test func selfTestPasses() async {
        var lines: [String] = []
        let checks = await SelfTest.run { lines.append($0) }
        #expect(checks.count == 8)
        // The cancel *latency* bound is enforced by `Darkslide --self-test` (run alone) and by
        // ZFFI; here the suite shares the rayon pool with hundreds of parallel renders, so only
        // correctness is asserted for that check.
        #expect(checks.filter { $0.name != "cancel" }.allSatisfy { $0.passed }, "\(lines)")
        #expect(checks.first { $0.name == "cancel" }.map { $0.detail.contains("cancelled after") } == true)
    }

    @Test func durationMilliseconds() {
        #expect(Duration.milliseconds(1500).inMilliseconds == 1500)
    }
}

@Suite struct RustRawTests {
    func meta(make: String = "SONY", model: String = "ILCE-9", time: CaptureTimeData? = nil) -> RawMetadata {
        RawMetadata(make: make, model: model, lens: "FE 85mm F1.4 GM", iso: 800, shutterSeconds: 0.004,
                    shutterDisplay: "1/250", aperture: 1.4, focalLength: 85, focalLength35mm: nil, captureTime: time,
                    orientation: 6, width: 4000, height: 6000, serial: nil, shutterCount: nil, afX: nil, afY: nil,
                    shotNumber: 3, sequenceImageNumber: 1)
    }

    @Test func cameraNames() {
        #expect(RustRaw.cameraName(make: "SONY", model: "ILCE-9") == "Sony ILCE-9")
        #expect(RustRaw.cameraName(make: "Canon", model: "Canon EOS R5") == "Canon EOS R5")
        #expect(RustRaw.cameraName(make: "", model: "X100V") == "X100V")
        #expect(RustRaw.cameraName(make: "FUJIFILM", model: "") == "Fujifilm")
        #expect(RustRaw.cameraName(make: "DJI", model: "FC3582") == "DJI FC3582", "short acronyms stay")
    }

    @Test func captureDatesUseOffsetOrLocalZone() {
        let utc = TimeZone(identifier: "UTC")!
        let t = CaptureTimeData(year: 2026, month: 8, day: 8, hour: 17, minute: 25, second: 40, nanos: 500_000_000,
                                utcOffsetMinutes: -240)
        let d = RustRaw.captureDate(t, fallback: .distantPast, timeZone: utc)
        #expect(d.timeIntervalSince1970 == 1_786_224_340.5)
        var local = t
        local.utcOffsetMinutes = nil
        #expect(RustRaw.captureDate(local, fallback: .distantPast, timeZone: utc).timeIntervalSince1970 == 1_786_209_940.5)
        #expect(RustRaw.captureDate(nil, fallback: .distantPast, timeZone: utc) == .distantPast)
        var bad = t
        bad.month = 13
        #expect(RustRaw.captureDate(bad, fallback: .distantPast, timeZone: utc) != .distantFuture)
    }

    @Test func makePhotoMapsTheRecord() {
        let url = URL(fileURLWithPath: "/x/DSC0001.ARW")
        let p = RustRaw.makePhoto(meta(), url: url, id: PhotoID(7), fileSize: 49_000_000, fallbackDate: .distantPast)
        #expect(p.id == PhotoID(7) && p.filename == "DSC0001.ARW" && p.url == url)
        #expect(p.meta.camera == "Sony ILCE-9" && p.meta.lens == "FE 85mm F1.4 GM" && p.meta.iso == 800)
        #expect(p.meta.shutter == 0.004 && abs(p.meta.aperture - 1.4) < 1e-6 && p.meta.focalLength == 85)
        #expect(p.meta.pixelWidth == 4000 && p.meta.pixelHeight == 6000 && p.meta.isPortrait)
        #expect(p.meta.captureDate == .distantPast && p.meta.fileSize == 49_000_000)
        #expect(p.rating == 0 && p.pick == .none && !p.isEdited)
        let empty = RawMetadata(make: "", model: "", lens: nil, iso: nil, shutterSeconds: nil, shutterDisplay: nil,
                                aperture: nil, focalLength: nil, focalLength35mm: nil, captureTime: nil, orientation: 1,
                                width: nil, height: nil, serial: nil, shutterCount: nil, afX: nil, afY: nil,
                                shotNumber: nil, sequenceImageNumber: nil)
        let q = RustRaw.makePhoto(empty, url: url, id: PhotoID(1), fileSize: 0, fallbackDate: .distantPast)
        #expect(q.meta.iso == 0 && q.meta.lens.isEmpty && q.meta.pixelWidth == 0)
    }

    @Test func unreadableFilesThrowTypedErrors() async throws {
        await #expect(throws: RenderError.self) {
            _ = try await RustRaw.photo(at: URL(fileURLWithPath: "/nonexistent/x.ARW"), id: PhotoID(1))
        }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-rawtests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let fake = dir.appendingPathComponent("fake.ARW")
        try Data("not a raw".utf8).write(to: fake)
        #expect(!RustRaw.isSupported(fake))
        do {
            _ = try await RustPreviewProvider().preview(for: fake, maxPixelSize: 256)
            Issue.record("expected an error")
        } catch let e as RenderError {
            #expect(e.kind == .unsupported)
        }
        let cancelled = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await RustRaw.photo(at: fake, id: PhotoID(1))
        }
        await #expect(throws: CancellationError.self) { try await cancelled.value }
        let cancelledPreview = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await RustPreviewProvider().preview(for: fake, maxPixelSize: 1)
        }
        await #expect(throws: CancellationError.self) { try await cancelledPreview.value }
    }

    @Test func realSampleMetadataAndPreview() async throws {
        guard let url = sampleARW() else { return } // samples/ absent: covered by the synthetic tests
        #expect(RustRaw.isSupported(url))
        let photo = try await RustRaw.photo(at: url, id: PhotoID(1))
        #expect(photo.meta.camera == "Sony ILCE-9")
        #expect(max(photo.meta.pixelWidth, photo.meta.pixelHeight) == 6000)
        #expect(photo.meta.captureDate > Date(timeIntervalSince1970: 1_500_000_000))
        let provider = RustPreviewProvider(cacheLimit: 4)
        let clock = ContinuousClock()
        let t0 = clock.now
        let img = try await provider.preview(for: url, maxPixelSize: 512)
        let ms = (clock.now - t0).inMilliseconds
        #expect(max(img.width, img.height) == 512)
        #expect((img.width > img.height) == (photo.meta.pixelWidth > photo.meta.pixelHeight), "oriented like the photo")
        // 200 ms is the budget (checked in the isolated perf pass, DS_PERF=1); the parallel
        // suite shares CPU and GPU with the develop tests (214-232 ms seen), so it only
        // guards against a full RAW decode sneaking in.
        #expect(ms < (ProcessInfo.processInfo.environment["DS_PERF"] != nil ? 200 : 1000), "preview in \(ms) ms")
        let again = try await provider.preview(for: url, maxPixelSize: 512)
        #expect(again === img, "cache hit")
        let full = try await provider.preview(for: url, maxPixelSize: 0)
        #expect(max(full.width, full.height) >= 1600)
    }
}

@Suite struct ImageOrientationTests {
    /// 3×2 image with a distinct gray level per pixel (row-major, top row first).
    func grid() -> CGImage {
        let w = 3, h = 2
        var px = [UInt8](repeating: 255, count: w * h * 4)
        for i in 0..<(w * h) { px[i * 4] = UInt8(i * 40); px[i * 4 + 1] = UInt8(i * 40); px[i * 4 + 2] = UInt8(i * 40) }
        let ctx = CGContext(data: &px, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        return ctx.makeImage()!
    }

    func values(_ img: CGImage) -> [[Int]] {
        let w = img.width, h = img.height
        var px = [UInt8](repeating: 0, count: w * h * 4)
        let ctx = CGContext(data: &px, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
        return (0..<h).map { y in (0..<w).map { x in Int(px[(y * w + x) * 4]) / 40 } }
    }

    @Test func everyExifOrientationMatchesTheSpec() {
        // Stored: [[0,1,2],[3,4,5]]. in(x, y) = y * 3 + x, W = 3, H = 2.
        let expected: [UInt16: [[Int]]] = [
            1: [[0, 1, 2], [3, 4, 5]],
            2: [[2, 1, 0], [5, 4, 3]],          // mirror horizontal
            3: [[5, 4, 3], [2, 1, 0]],          // rotate 180
            4: [[3, 4, 5], [0, 1, 2]],          // mirror vertical
            5: [[0, 3], [1, 4], [2, 5]],        // transpose
            6: [[3, 0], [4, 1], [5, 2]],        // rotate 90 CW
            7: [[5, 2], [4, 1], [3, 0]],        // transverse
            8: [[2, 5], [1, 4], [0, 3]],        // rotate 90 CCW
        ]
        for (o, want) in expected.sorted(by: { $0.key < $1.key }) {
            let out = ImageOrientation.apply(exif: o, to: grid())!
            #expect(values(out) == want, "orientation \(o)")
        }
        #expect(ImageOrientation.apply(exif: 0, to: grid()).map(values) == expected[1])
        #expect(ImageOrientation.orientedSize(exif: 6, width: 3, height: 2) == (2, 3))
    }

    @Test func decodeRejectsGarbageAndDownscales() throws {
        #expect(RustPreviewProvider.decode(jpeg: Data([1, 2, 3]), maxPixelSize: 10, exifOrientation: 1) == nil)
        let data = NSMutableData()
        let dest = CGImageDestinationCreateWithData(data, "public.jpeg" as CFString, 1, nil)!
        let big = CGContext(data: nil, width: 400, height: 200, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        CGImageDestinationAddImage(dest, big.makeImage()!, nil)
        #expect(CGImageDestinationFinalize(dest))
        let small = try #require(RustPreviewProvider.decode(jpeg: data as Data, maxPixelSize: 100, exifOrientation: 8))
        #expect(small.width == 50 && small.height == 100, "downscaled then rotated")
        let full = try #require(RustPreviewProvider.decode(jpeg: data as Data, maxPixelSize: 0, exifOrientation: 1))
        #expect(full.width == 400)
    }

    @Test func previewCacheEvictsLeastRecentlyUsed() {
        let c = PreviewCache(limit: 2)
        let img = grid()
        c.set("a", img)
        c.set("b", img)
        #expect(c.get("a") != nil)
        c.set("c", img)
        #expect(c.get("b") == nil && c.get("a") != nil && c.get("c") != nil && c.count == 2)
        c.set("c", img)
        #expect(c.count == 2)
        #expect(PreviewCache(limit: 0).limit == 1)
    }

    @Test func frameDestinationFreesHeapOnDiscard() throws {
        let d = try FrameDestination(length: 0, device: nil)
        #expect(d.length == 1 && !d.isMetal)
        d.discard()
        d.discard()
        let m = try FrameDestination(length: 64, device: MTLCreateSystemDefaultDevice())
        #expect(m.isMetal)
    }
}
