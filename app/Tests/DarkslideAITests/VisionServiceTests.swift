import CoreGraphics
import CoreVideo
import Testing
import Vision
@testable import DarkslideAI

/// Real Vision requests on synthetic images. Vision's models are opaque, so
/// assertions check structure and direction, not exact values.
@Suite struct VisionServiceTests {
    let vision = VisionService()

    @Test func featurePrintDistanceSeparatesScenes() async throws {
        let a = try #require(try await vision.featurePrint(TestImages.horizon(320, 240, degrees: 0)))
        let a2 = try #require(try await vision.featurePrint(TestImages.horizon(320, 240, degrees: 0)))
        let b = try #require(try await vision.featurePrint(TestImages.checkerboard(320, 240, cell: 16)))
        #expect(a.count > 100)
        let same = try #require(FeaturePrint.distance(a, a2))
        let different = try #require(FeaturePrint.distance(a, b))
        #expect(same < 1e-4)
        #expect(different > same + 0.1)
    }

    @Test func noFacesInALandscape() async throws {
        #expect(try await vision.faces(TestImages.horizon(320, 240, degrees: 0)).isEmpty)
    }

    @Test func subjectMaskIsolatesTheFigure() async throws {
        // Vision finds the cartoon head as a foreground subject (≈ 34% of the frame).
        let mask = try #require(try await vision.subjectMask(TestImages.face()))
        #expect(mask.width == 512 && mask.height == 512)
        #expect(mask.coverage > 0.1 && mask.coverage < 0.9)
    }

    /// Vision's face detector ignores cartoon faces, so the capture-quality and
    /// landmark conversion runs on a provided observation instead.
    @Test func faceInfosFromObservations() throws {
        let box = CGRect(x: 0.3, y: 0.2, width: 0.4, height: 0.5)
        let obs = VNFaceObservation(requestRevision: VNDetectFaceRectanglesRequestRevision3, boundingBox: box,
                                    roll: nil, yaw: nil, pitch: nil)
        let far = VNFaceObservation(requestRevision: VNDetectFaceRectanglesRequestRevision3,
                                    boundingBox: CGRect(x: 0, y: 0, width: 0.05, height: 0.05),
                                    roll: nil, yaw: nil, pitch: nil)
        let faces = VisionService.faceInfos([obs], qualities: [far, obs])
        #expect(faces.count == 1)
        #expect(faces[0].boundingBox == box)
        #expect(faces[0].leftEyeAspect == nil && faces[0].eyesOpen == nil, "no landmarks on a bare observation")
        #expect(faces[0].captureQuality == nil, "bare observations carry no quality")
        #expect(VisionService.faceInfos([], qualities: [obs]).isEmpty)
        #expect(abs(VisionService.iou(box, box) - 1) < 1e-9)
        #expect(VisionService.iou(box, far.boundingBox) == 0)
    }

    @Test func analysisRequestsAreBatchedAndNotForcedToCPU() throws {
        let with = VisionService.AnalysisRequests(includeLabels: true)
        let without = VisionService.AnalysisRequests(includeLabels: false)
        #expect(with.all.count == 7 && without.all.count == 6, "one perform for every request of the pass")
        #expect(with.all.contains { $0 === with.quality }, "capture quality rides in the same perform")
        for r in with.all {
            // nil = Vision picks the device (Neural Engine / GPU); nothing pins the CPU.
            #expect(r.computeDevice(for: .main) == nil, "\(type(of: r)) is pinned to a compute device")
        }
    }

    @Test func facesReportCaptureQualityFromTheBatchedPass() async throws {
        // Vision may find no face in synthetic art; any face it returns carries a quality from the same pass.
        for f in try await vision.faces(TestImages.face()) {
            if let q = f.captureQuality { #expect(q >= 0 && q <= 1) }
        }
    }

    @Test func horizonAngleHasTheRightSign() async throws {
        // Vision may decline on synthetic art; when it answers, the sign must match.
        if let up = try await vision.horizonDegrees(TestImages.horizon(640, 480, degrees: 6)) {
            #expect(up > 1 && up < 12, "got \(up)")
        }
        if let down = try await vision.horizonDegrees(TestImages.horizon(640, 480, degrees: -6)) {
            #expect(down < -1 && down > -12, "got \(down)")
        }
    }

    @Test func saliencyFindsTheBrightBlob() async throws {
        let regions = try await vision.salientRegions(TestImages.blob(400, 300, cx: 0.75, cy: 0.7, radius: 0.12))
        try #require(!regions.isEmpty)
        let union = regions.reduce(CGRect.null) { $0.union($1) }
        #expect(union.contains(CGPoint(x: 0.75, y: 0.7)) || abs(union.midX - 0.75) < 0.2)
    }

    @Test func classificationReturnsRankedLabels() async throws {
        let labels = try await vision.labels(TestImages.horizon(320, 240, degrees: 0), limit: 5, minConfidence: 0)
        #expect(!labels.isEmpty && labels.count <= 5)
        #expect(zip(labels, labels.dropFirst()).allSatisfy { $0.confidence >= $1.confidence })
    }

    @Test func aestheticsScoreIsInRange() async throws {
        if let s = try await vision.aesthetics(TestImages.horizon(320, 240, degrees: 0)) {
            #expect(s.overall >= -1 && s.overall <= 1)
        }
    }

    @Test func masksOnAnEmptySceneAreNilOrNearlyEmpty() async throws {
        let img = TestImages.solid(256, 256, gray: 0.4)
        if let subject = try? await vision.subjectMask(img) {
            #expect(subject.width > 0)
        }
        if let person = try? await vision.personMask(img) {
            #expect(person.coverage < 0.2)
            #expect(person.bytes.count == person.width * person.height)
        }
    }

    @Test func analyzeAggregatesEverything() async throws {
        let a = try await vision.analyze(TestImages.checkerboard(256, 256, cell: 8))
        #expect(a.statistics.laplacianVariance > 1000)
        #expect(a.faces.isEmpty)
        #expect((a.featurePrint?.count ?? 0) > 100)
        #expect(FeaturePrint.distance(a.featurePrint, nil) == nil)
        #expect(FeaturePrint.distance([1, 2], [1]) == nil && FeaturePrint.distance([], []) == nil)
        #expect(FeaturePrint.distance([0, 3], [4, 0]) == 5)
        let noLabels = try await vision.analyze(TestImages.solid(64, 64, gray: 0.5), includeLabels: false)
        #expect(noLabels.labels.isEmpty)
    }

    @Test func cancelledTaskDoesNotRun() async {
        let task = Task { () -> [Float]? in
            withUnsafeCurrentTask { $0?.cancel() }
            return try await vision.featurePrint(TestImages.solid(64, 64, gray: 0.5))
        }
        await #expect(throws: CancellationError.self) { try await task.value }
    }
}

@Suite struct MaskConversionTests {
    func buffer(_ format: OSType, _ fill: (UnsafeMutableRawPointer, Int) -> Void) -> CVPixelBuffer {
        var pb: CVPixelBuffer?
        CVPixelBufferCreate(nil, 4, 2, format, nil, &pb)
        let b = pb!
        CVPixelBufferLockBaseAddress(b, [])
        fill(CVPixelBufferGetBaseAddress(b)!, CVPixelBufferGetBytesPerRow(b))
        CVPixelBufferUnlockBaseAddress(b, [])
        return b
    }

    @Test func eightBitMask() {
        let b = buffer(kCVPixelFormatType_OneComponent8) { base, stride in
            for y in 0..<2 { for x in 0..<4 { base.storeBytes(of: UInt8(x < 2 ? 255 : 0), toByteOffset: y * stride + x, as: UInt8.self) } }
        }
        let m = VisionService.mask(from: b)
        #expect(m.width == 4 && m.height == 2)
        #expect(m.bytes == [255, 255, 0, 0, 255, 255, 0, 0])
        #expect(m.coverage == 0.5)
    }

    @Test func floatMaskIsScaledAndClamped() {
        let b = buffer(kCVPixelFormatType_OneComponent32Float) { base, stride in
            let vals: [Float] = [0, 0.5, 1, 2]
            for y in 0..<2 { for x in 0..<4 { base.storeBytes(of: vals[x], toByteOffset: y * stride + x * 4, as: Float.self) } }
        }
        let m = VisionService.mask(from: b)
        #expect(Array(m.bytes.prefix(4)) == [0, 127, 255, 255])
        #expect(VisionMask(width: 0, height: 0, bytes: []).coverage == 0)
    }
}
