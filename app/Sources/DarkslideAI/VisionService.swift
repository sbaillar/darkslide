import CoreGraphics
import CoreVideo
import Foundation
import Synchronization
import Vision

/// An 8-bit single-channel mask (0 = background, 255 = selected), row-major.
public struct VisionMask: Sendable, Equatable {
    public let width: Int
    public let height: Int
    public let bytes: [UInt8]
    /// This content's identity for the develop pipeline's raster cache: new for every
    /// mask made (a regenerated Vision or sky mask gets a new one); copies keep it.
    public let version: UInt64

    private static let generation = Atomic<UInt64>(0)

    public init(width: Int, height: Int, bytes: [UInt8]) {
        self.width = width
        self.height = height
        self.bytes = bytes
        version = Self.generation.add(1, ordering: .relaxed).newValue
    }

    /// Masks are equal by content (two generations of the same pixels are equal).
    public static func == (a: VisionMask, b: VisionMask) -> Bool {
        a.width == b.width && a.height == b.height && a.bytes == b.bytes
    }

    /// Fraction of the frame that is (at least half) selected.
    public var coverage: Double {
        guard !bytes.isEmpty else { return 0 }
        return Double(bytes.reduce(0) { $0 + ($1 >= 128 ? 1 : 0) }) / Double(bytes.count)
    }
}

public struct ClassificationLabel: Sendable, Equatable {
    public var identifier: String
    public var confidence: Double

    public init(identifier: String, confidence: Double) {
        self.identifier = identifier
        self.confidence = confidence
    }
}

public struct AestheticsScore: Sendable, Equatable {
    /// -1 (poor) ... 1 (great).
    public var overall: Double
    public var isUtility: Bool

    public init(overall: Double, isUtility: Bool) {
        self.overall = overall
        self.isUtility = isUtility
    }
}

/// Everything `analyze` gathers in one Vision pass.
public struct ImageAnalysis: Sendable, Equatable {
    public var statistics: ImageStatistics
    public var faces: [FaceInfo]
    public var aesthetics: AestheticsScore?
    public var featurePrint: [Float]?
    public var labels: [ClassificationLabel]
    /// Degrees; positive = the horizon rises to the right (counter-clockwise tilt).
    public var horizonDegrees: Double?
    /// Normalized, Vision coordinates (origin bottom-left).
    public var salientRegions: [CGRect]

    public init(statistics: ImageStatistics, faces: [FaceInfo], aesthetics: AestheticsScore?, featurePrint: [Float]?,
                labels: [ClassificationLabel], horizonDegrees: Double?, salientRegions: [CGRect]) {
        self.statistics = statistics
        self.faces = faces
        self.aesthetics = aesthetics
        self.featurePrint = featurePrint
        self.labels = labels
        self.horizonDegrees = horizonDegrees
        self.salientRegions = salientRegions
    }
}

/// Vision feature-print comparison (culling scores and grouping live in ds-ai).
public enum FeaturePrint {
    /// Euclidean distance; `nil` when either is missing or lengths differ.
    public static func distance(_ a: [Float]?, _ b: [Float]?) -> Float? {
        guard let a, let b, a.count == b.count, !a.isEmpty else { return nil }
        var s: Float = 0
        for i in a.indices {
            let d = a[i] - b[i]
            s += d * d
        }
        return s.squareRoot()
    }
}

public enum VisionServiceError: Error, Equatable {
    case emptyImage
}

/// On-device Apple Vision analysis on `CGImage`s.
///
/// Vision's `perform` is synchronous and can take tens of milliseconds, so
/// every call runs on a private dispatch queue instead of a Swift
/// cooperative-pool thread (which it would otherwise block). Vision request and
/// observation objects never leave that queue; only `Sendable` values do.
public struct VisionService: Sendable {
    private static let queue = DispatchQueue(label: "casa.baillargeon.darkslide.vision", qos: .userInitiated,
                                             attributes: .concurrent)

    public init() {}

    /// At most this many Vision `perform` calls run at once, process-wide. Vision runs
    /// dependent requests (aesthetics → its feature print, …) through its own
    /// capacity-limited queues and *blocks* a thread waiting for capacity; with an
    /// unbounded number of concurrent performs (AI Cull batches, Auto, masks and
    /// thumbnails at once) every GCD worker ended up blocked and the process hung (seen
    /// in the full test suite). Waiting here is async: it holds no thread.
    static let limiter = AsyncLimiter(ProcessInfo.processInfo.activeProcessorCount >= 8 ? 4 : 2)

    static func run<T: Sendable>(_ work: @escaping @Sendable () throws -> T) async throws -> T {
        try Task.checkCancellation()
        await limiter.acquire()
        let value: T
        do {
            value = try await withCheckedThrowingContinuation { (c: CheckedContinuation<T, Error>) in
                queue.async { c.resume(with: Result { try work() }) }
            }
        } catch {
            await limiter.release()
            throw error
        }
        await limiter.release()
        try Task.checkCancellation()
        return value
    }

    // MARK: - Individual requests

    /// Subject (foreground instance) mask at image resolution, all instances merged.
    /// `nil` when Vision finds no subject.
    public func subjectMask(_ image: CGImage) async throws -> VisionMask? {
        try await Self.run {
            let handler = VNImageRequestHandler(cgImage: image)
            let req = VNGenerateForegroundInstanceMaskRequest()
            try handler.perform([req])
            guard let obs = req.results?.first, !obs.allInstances.isEmpty else { return nil }
            let buf = try obs.generateScaledMaskForImage(forInstances: obs.allInstances, from: handler)
            return Self.mask(from: buf)
        }
    }

    /// Person segmentation mask (all people), at Vision's output resolution.
    public func personMask(_ image: CGImage) async throws -> VisionMask? {
        try await Self.run {
            let handler = VNImageRequestHandler(cgImage: image)
            let req = VNGeneratePersonSegmentationRequest()
            req.qualityLevel = .balanced
            req.outputPixelFormat = kCVPixelFormatType_OneComponent8
            try handler.perform([req])
            return req.results?.first.map { Self.mask(from: $0.pixelBuffer) }
        }
    }

    public func faces(_ image: CGImage) async throws -> [FaceInfo] {
        try await Self.run {
            let handler = VNImageRequestHandler(cgImage: image)
            let landmarks = VNDetectFaceLandmarksRequest()
            let quality = VNDetectFaceCaptureQualityRequest()
            try handler.perform([landmarks, quality]) // one batched pass
            return Self.faceInfos(landmarks.results ?? [], qualities: quality.results ?? [])
        }
    }

    public func aesthetics(_ image: CGImage) async throws -> AestheticsScore? {
        try await Self.run {
            let req = VNCalculateImageAestheticsScoresRequest()
            try VNImageRequestHandler(cgImage: image).perform([req])
            return Self.aesthetics(req.results?.first)
        }
    }

    public func featurePrint(_ image: CGImage) async throws -> [Float]? {
        try await Self.run {
            let req = VNGenerateImageFeaturePrintRequest()
            try VNImageRequestHandler(cgImage: image).perform([req])
            return req.results?.first.flatMap(Self.vector)
        }
    }

    /// Top `limit` labels at or above `minConfidence`, most confident first.
    public func labels(_ image: CGImage, limit: Int = 8, minConfidence: Double = 0.3) async throws -> [ClassificationLabel] {
        try await Self.run {
            let req = VNClassifyImageRequest()
            try VNImageRequestHandler(cgImage: image).perform([req])
            return Self.labels(req.results ?? [], limit: limit, minConfidence: minConfidence)
        }
    }

    /// Horizon tilt in degrees (positive = rises to the right), or `nil` when no
    /// horizon is found. Vision's `angle` is the *correcting* rotation, i.e. the
    /// negative of the tilt (verified on synthetic ±6° horizons).
    public func horizonDegrees(_ image: CGImage) async throws -> Double? {
        try await Self.run {
            let req = VNDetectHorizonRequest()
            try VNImageRequestHandler(cgImage: image).perform([req])
            return req.results?.first.map { -Double($0.angle) * 180 / .pi }
        }
    }

    /// Attention-based salient regions (normalized, origin bottom-left).
    public func salientRegions(_ image: CGImage) async throws -> [CGRect] {
        try await Self.run {
            let req = VNGenerateAttentionBasedSaliencyImageRequest()
            try VNImageRequestHandler(cgImage: image).perform([req])
            return (req.results?.first?.salientObjects ?? []).map(\.boundingBox)
        }
    }

    // MARK: - Aggregate

    /// Pixel statistics plus faces, aesthetics, feature print, labels, horizon,
    /// and saliency in a single handler pass (Vision shares the decoded image).
    public func analyze(_ image: CGImage, includeLabels: Bool = true) async throws -> ImageAnalysis {
        try await Self.run {
            guard let stats = ImageStatistics(image: image) else { throw VisionServiceError.emptyImage }
            let handler = VNImageRequestHandler(cgImage: image)
            let r = AnalysisRequests(includeLabels: includeLabels)
            try handler.perform(r.all) // every request in one batched pass over the decoded image
            let landmarks = r.landmarks, aesthetics = r.aesthetics, print = r.print, horizon = r.horizon
            let saliency = r.saliency, classify = r.classify
            return ImageAnalysis(
                statistics: stats,
                faces: Self.faceInfos(landmarks.results ?? [], qualities: r.quality.results ?? []),
                aesthetics: Self.aesthetics(aesthetics.results?.first),
                featurePrint: print.results?.first.flatMap(Self.vector),
                labels: includeLabels ? Self.labels(classify.results ?? [], limit: 8, minConfidence: 0.3) : [],
                horizonDegrees: horizon.results?.first.map { -Double($0.angle) * 180 / .pi },
                salientRegions: (saliency.results?.first?.salientObjects ?? []).map(\.boundingBox))
        }
    }

    /// The requests of one `analyze` pass. Vision picks the compute device
    /// (Neural Engine / GPU); nothing here forces CPU-only.
    struct AnalysisRequests {
        let landmarks = VNDetectFaceLandmarksRequest()
        let quality = VNDetectFaceCaptureQualityRequest()
        let aesthetics = VNCalculateImageAestheticsScoresRequest()
        let print = VNGenerateImageFeaturePrintRequest()
        let horizon = VNDetectHorizonRequest()
        let saliency = VNGenerateAttentionBasedSaliencyImageRequest()
        let classify = VNClassifyImageRequest()
        let includeLabels: Bool

        var all: [VNRequest] {
            let base: [VNRequest] = [landmarks, quality, aesthetics, print, horizon, saliency]
            return includeLabels ? base + [classify] : base
        }
    }

    /// Pair landmark faces with capture-quality faces (same detector; matched by box overlap).
    static func faceInfos(_ faces: [VNFaceObservation], qualities: [VNFaceObservation]) -> [FaceInfo] {
        faces.map { f in
            let best = qualities.max { iou($0.boundingBox, f.boundingBox) < iou($1.boundingBox, f.boundingBox) }
            let q = best.flatMap { iou($0.boundingBox, f.boundingBox) > 0.3 ? $0.faceCaptureQuality.map(Double.init) : nil }
            return FaceInfo(boundingBox: f.boundingBox, captureQuality: q,
                            leftEyeAspect: f.landmarks?.leftEye.flatMap { EyeMetrics.aspectRatio($0.normalizedPoints) },
                            rightEyeAspect: f.landmarks?.rightEye.flatMap { EyeMetrics.aspectRatio($0.normalizedPoints) })
        }
    }

    static func iou(_ a: CGRect, _ b: CGRect) -> Double {
        let i = a.intersection(b)
        guard !i.isNull, i.width > 0, i.height > 0 else { return 0 }
        let inter = Double(i.width * i.height)
        return inter / (Double(a.width * a.height) + Double(b.width * b.height) - inter)
    }

    // MARK: - Observation → value conversion (runs on the Vision queue)

    private static func aesthetics(_ obs: VNImageAestheticsScoresObservation?) -> AestheticsScore? {
        obs.map { AestheticsScore(overall: Double($0.overallScore), isUtility: $0.isUtility) }
    }

    static func labels(_ obs: [VNClassificationObservation], limit: Int, minConfidence: Double) -> [ClassificationLabel] {
        obs.filter { Double($0.confidence) >= minConfidence }
            .sorted { $0.confidence > $1.confidence }
            .prefix(limit)
            .map { ClassificationLabel(identifier: $0.identifier, confidence: Double($0.confidence)) }
    }

    static func vector(_ fp: VNFeaturePrintObservation) -> [Float]? {
        let n = fp.elementCount
        guard n > 0 else { return nil }
        return fp.data.withUnsafeBytes { raw -> [Float] in
            switch fp.elementType {
            case .double: return Array(raw.bindMemory(to: Double.self).prefix(n)).map(Float.init)
            default: return Array(raw.bindMemory(to: Float.self).prefix(n))
            }
        }
    }

    /// Convert a one-channel 8-bit or 32-bit float pixel buffer to an 8-bit mask.
    static func mask(from buffer: CVPixelBuffer) -> VisionMask {
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        let (w, h) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer))
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        guard let base = CVPixelBufferGetBaseAddress(buffer) else { return VisionMask(width: w, height: h, bytes: []) }
        var out = [UInt8](repeating: 0, count: w * h)
        let isFloat = CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_OneComponent32Float
        for y in 0..<h {
            let row = base.advanced(by: y * stride)
            for x in 0..<w {
                out[y * w + x] = isFloat
                    ? UInt8(min(max(row.load(fromByteOffset: x * 4, as: Float.self), 0), 1) * 255)
                    : row.load(fromByteOffset: x, as: UInt8.self)
            }
        }
        return VisionMask(width: w, height: h, bytes: out)
    }
}

/// A counting limiter for async work: `acquire` suspends (holding no thread) while
/// `limit` holders are active; `release` wakes the longest waiter.
public actor AsyncLimiter {
    private let limit: Int
    private var active = 0
    private var waiters: [CheckedContinuation<Void, Never>] = []

    public init(_ limit: Int) { self.limit = max(1, limit) }

    public func acquire() async {
        if active < limit {
            active += 1
            return
        }
        await withCheckedContinuation { waiters.append($0) }
    }

    public func release() {
        if waiters.isEmpty {
            active = max(0, active - 1)
        } else {
            waiters.removeFirst().resume() // the slot passes straight to the waiter
        }
    }

    /// Holders right now (tests).
    public var holders: Int { active }
}
