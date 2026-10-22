import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation

/// `AIService` on ds-ai (Rust) + Apple Vision + Foundation Models, all on-device.
///
/// Culling: each photo's preview (≈1616 px, the embedded JPEG in live mode) is
/// read once, Vision measures faces/eyes, aesthetics and saliency on it, and
/// ds-ai analyses the pixels (sharpness on the subject, exposure, noise,
/// perceptual hash). ds-ai then groups the frames into bursts/moments (shot
/// numbers resolve the A9's whole-second timestamps) and scores each frame
/// against its group. Previews are analysed by ds-ai in batches of `batchSize`
/// (`add_batch`, the GPU analyzer) while Vision works on the next ones; at most
/// `maxConcurrent` + 3 batches of previews are in memory at once;
/// Rust keeps only the compact analyses.
public struct VisionAIService: AIService {
    public let vision: VisionService
    public let language: any NaturalLanguageEditService
    public let maxConcurrent: Int
    /// Previews per `CullSession.add_batch` call (the GPU analyzer's sweet spot is
    /// 16–32); 1 analyses each preview on its own (`add`).
    public let batchSize: Int
    public static let thumbnailSize = 512

    public init(vision: VisionService = VisionService(),
                language: any NaturalLanguageEditService = GuardedEditService(models: [FoundationModelsEditService()]),
                maxConcurrent: Int = 8, batchSize: Int = 24) {
        self.vision = vision
        self.language = language
        self.maxConcurrent = max(1, maxConcurrent)
        self.batchSize = max(1, batchSize)
    }

    public func cull(_ photos: [Photo], images: any ImageProvider,
                     progress: @escaping @Sendable (Int, Int) -> Void) async throws -> [CullSuggestion] {
        let session = try CullSession()
        let vision = vision
        let batchSize = batchSize
        // Vision + pixel extraction run `maxConcurrent` at a time. With `batchSize` > 1 the
        // pixels go to ds-ai in batches (`add_batch`: one GPU dispatch per batch), each
        // batch analysed while Vision keeps working on the next photos (≤ 2 in flight).
        enum Step: Sendable { case input(CullInput?), analysed(Int) }
        try await withThrowingTaskGroup(of: Step.self) { group in
            var next = 0
            var visionOutstanding = 0
            func addNext() {
                guard next < photos.count else { return }
                let photo = photos[next]
                next += 1
                visionOutstanding += 1
                group.addTask {
                    do {
                        let input = try await Self.input(for: photo, images: images, vision: vision)
                        guard batchSize == 1 else { return .input(input) }
                        try await Self.add([input], to: session, batchSize: 1)
                        return .analysed(1)
                    } catch is CancellationError {
                        throw CancellationError()
                    } catch {
                        // One unreadable image must not fail the whole cull; it stays undecided.
                        return batchSize == 1 ? .analysed(1) : .input(nil)
                    }
                }
            }
            var pending: [CullInput] = []
            var inFlight = 0
            // At most `batchSize` previews per call (a whole preview list in one FFI buffer
            // must stay far below its 2 GB limit), and Vision waits while two batches'
            // worth are queued, so memory stays bounded (about 3 batches of 7 MB previews).
            func startBatch() {
                let batch = Array(pending.prefix(batchSize))
                pending.removeFirst(batch.count)
                inFlight += 1
                group.addTask {
                    try await Self.add(batch, to: session, batchSize: batchSize)
                    return .analysed(batch.count)
                }
            }
            for _ in 0..<min(maxConcurrent, photos.count) { addNext() }
            var done = 0, skipped = 0
            while let step = try await group.next() {
                switch step {
                case .input(let input):
                    visionOutstanding -= 1
                    if let input { pending.append(input) } else { skipped += 1 }
                case .analysed(let n):
                    if batchSize > 1 { inFlight -= 1 } else { visionOutstanding -= 1 }
                    done += n
                    progress(min(done + skipped, photos.count), photos.count)
                }
                let visionDone = next >= photos.count && visionOutstanding == 0
                while !pending.isEmpty, inFlight < 2, pending.count >= batchSize || visionDone { startBatch() }
                while visionOutstanding < maxConcurrent, next < photos.count,
                      batchSize == 1 || pending.count < 2 * batchSize { addNext() }
            }
            if skipped > 0, done + skipped == photos.count { progress(photos.count, photos.count) }
        }
        let token = try CancelToken()
        let report = try await withTaskCancellationHandler {
            try await session.finish(token: token)
        } onCancel: { try? token.cancel() }
        return DSAI.suggestions(report, photos: photos)
    }

    /// Vision analysis + the pixels of one photo's preview, ready for ds-ai.
    static func input(for photo: Photo, images: any ImageProvider, vision: VisionService) async throws -> CullInput {
        let image = try await images.thumbnail(for: photo, maxPixelSize: DSAI.analysisSize)
        try Task.checkCancellation()
        let analysis = try? await vision.analyze(image, includeLabels: false)
        guard let px = DSAI.rgba(image) else { throw VisionServiceError.emptyImage }
        return CullInput(id: photo.id.raw, width: UInt32(px.width), height: UInt32(px.height), rgba: px.bytes,
                         captureMs: DSAI.captureMs(photo.meta.captureDate), seq: DSAI.seq(photo.meta),
                         camera: photo.meta.camera.isEmpty ? nil : photo.meta.camera,
                         vision: DSAI.signals(analysis, afPoint: photo.meta.afPoint))
    }

    /// Hand `inputs` to ds-ai: one `add_batch` call, or one `add` each when `batchSize` is 1.
    /// A batch that fails as a whole (one malformed preview) falls back to one add per
    /// preview so the rest are still analysed.
    static func add(_ inputs: [CullInput], to session: CullSession, batchSize: Int) async throws {
        let token = try CancelToken()
        try await withTaskCancellationHandler {
            if batchSize > 1 {
                do {
                    _ = try await session.addBatch(inputs: inputs, token: token)
                    return
                } catch {
                    if Task.isCancelled { throw CancellationError() }
                }
            }
            for input in inputs {
                try Task.checkCancellation()
                _ = try? await session.add(input: input, token: token)
            }
        } onCancel: { try? token.cancel() }
    }

    public func autoAdjust(_ photo: Photo, current: EditState, images: any ImageProvider) async throws -> EditState {
        let thumb = try await images.thumbnail(for: photo, maxPixelSize: Self.thumbnailSize)
        guard let stats = ImageStatistics(image: thumb) else { return current }
        return AutoAdjust.apply(statistics: stats, to: current) // never straightens
    }

    public func applyInstruction(_ text: String, to current: EditState) async throws -> EditState {
        try await applyInstruction(text, to: current, asShot: nil)
    }

    public func applyInstruction(_ text: String, to current: EditState, asShot: WhiteBalanceValue?) async throws -> EditState {
        let intent = try await language.intent(for: text)
        return current.applying(intent, asShot: asShot)
    }

    /// Keyword suggestions for the Metadata tab (Vision image classification).
    public func keywords(for photo: Photo, images: any ImageProvider, limit: Int = 6) async throws -> [String] {
        let thumb = try await images.thumbnail(for: photo, maxPixelSize: Self.thumbnailSize)
        return try await vision.labels(thumb, limit: limit).map { $0.identifier.replacingOccurrences(of: "_", with: " ") }
    }
}
