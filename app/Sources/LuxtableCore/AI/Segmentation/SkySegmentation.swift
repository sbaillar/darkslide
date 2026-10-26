import CoreML
import LuxtableAI
import Foundation
import Observation
import Vision

public enum SegmentationError: Error, Equatable, LocalizedError {
    case download(String)
    case checksum(String)
    case compile(String)
    case inference(String)

    public var errorDescription: String? {
        switch self {
        case .download(let m): "Couldn't download the sky model: \(m)"
        case .checksum(let f): "The downloaded sky model is damaged (\(f)). Try again."
        case .compile(let m): "Couldn't prepare the sky model: \(m)"
        case .inference(let m): "Sky detection failed: \(m)"
        }
    }
}

/// The sky mask: a Core ML segmentation model (`DownloadableModelDescriptor.sky`), downloaded
/// into the app's support folder on first use (checksummed, with progress), compiled on this
/// Mac, then run through Vision on each photo's preview.
@MainActor @Observable
public final class SkySegmentation {
    public typealias State = DownloadableModel.State

    /// Label to mask (matched case-insensitively as a prefix in the model's class list).
    static let label = "sky"
    /// Class index used when the model carries no label list.
    static let fallbackIndex: Int32 = 187

    /// Downloads into the support folder. `LT_SKY_MODEL_DIR` (a local copy of the
    /// `.mlpackage`) replaces the network for development and screenshots.
    public static let shared = SkySegmentation(model: .standard(.sky))

    public let model: DownloadableModel

    public init(model: DownloadableModel) {
        self.model = model
    }

    public convenience init(root: URL, descriptor: DownloadableModelDescriptor = .sky,
                            downloader: any ModelDownloader = URLSessionModelDownloader(),
                            compile: (@Sendable (URL) async throws -> URL)? = nil,
                            load: (@Sendable (URL, MLModelConfiguration) throws -> MLModel)? = nil) {
        self.init(model: DownloadableModel(root: root, descriptor: descriptor, downloader: downloader,
                                           compile: compile, load: load))
    }

    public var state: State { model.state }
    public var descriptor: DownloadableModelDescriptor { model.descriptor }
    /// Where the compiled model lives.
    public var compiledURL: URL { model.compiledURL }

    /// The compiled model, downloading and compiling it on first use (one at a time).
    /// `cancel()` stops a download: this then throws `CancellationError` and the state
    /// goes back to `.notDownloaded`.
    public func prepare() async throws -> URL { try await model.prepare() }

    /// Stop the download in progress (the Layers panel's Cancel); nothing is kept.
    public func cancel() { model.cancel() }

    /// The sky mask of `image` (nil when there's no sky), downloading the model if needed.
    public func mask(for image: CGImage) async throws -> VisionMask? {
        let loaded = try await model.loadedModel()
        let index = Self.classIndex(labels: Self.labels(of: loaded), label: Self.label) ?? Self.fallbackIndex
        let job = SegmentJob(image: image, model: loaded, classIndex: index)
        return try await Task.detached(priority: .userInitiated) { try job.run() }.value
    }

    /// Class labels from the model's creator metadata (Apple's preview params).
    nonisolated static func labels(of model: MLModel) -> [String] {
        let meta = model.modelDescription.metadata[.creatorDefinedKey] as? [String: String] ?? [:]
        guard let params = meta["com.apple.coreml.model.preview.params"], let data = params.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let labels = obj["labels"] as? [String] else { return [] }
        return labels
    }

    /// Index of the first label starting with `label` (case-insensitive).
    nonisolated static func classIndex(labels: [String], label: String) -> Int32? {
        labels.firstIndex { $0.lowercased().hasPrefix(label.lowercased()) }.map(Int32.init)
    }

    nonisolated static func segment(_ image: CGImage, model: MLModel, classIndex: Int32) throws -> VisionMask? {
        let vnModel: VNCoreMLModel
        do {
            vnModel = try VNCoreMLModel(for: model)
        } catch {
            throw SegmentationError.inference(error.localizedDescription)
        }
        let request = VNCoreMLRequest(model: vnModel)
        request.imageCropAndScaleOption = .scaleFill // the mask then aligns with the whole frame
        do {
            try VNImageRequestHandler(cgImage: image).perform([request])
        } catch {
            throw SegmentationError.inference(error.localizedDescription)
        }
        guard let obs = request.results?.first as? VNCoreMLFeatureValueObservation,
              let array = obs.featureValue.multiArrayValue else { return nil }
        return classMask(array, classIndex: classIndex)
    }

    /// 0/255 mask of `classIndex` in a (H × W) class-id array, edges softened by a 3×3 box.
    nonisolated static func classMask(_ array: MLMultiArray, classIndex: Int32) -> VisionMask? {
        let shape = array.shape.map(\.intValue)
        guard shape.count >= 2 else { return nil }
        let h = shape[shape.count - 2], w = shape[shape.count - 1]
        guard w > 0, h > 0 else { return nil }
        guard array.count >= w * h else { return nil }
        // read the class ids through the raw buffer (per-element
        // subscripts box an NSNumber each: 12.8 ms -> 0.08 ms for 448x448).
        var hard = [UInt8](repeating: 0, count: w * h)
        var any = false
        func scan<T>(_ type: T.Type, _ matches: (T) -> Bool) {
            let p = array.dataPointer.bindMemory(to: T.self, capacity: w * h)
            for i in 0..<(w * h) where matches(p[i]) {
                hard[i] = 255
                any = true
            }
        }
        switch array.dataType {
        case .int32: scan(Int32.self) { $0 == classIndex }
        case .float16: scan(Float16.self) { Int32($0.rounded()) == classIndex }
        case .float32: scan(Float32.self) { Int32($0.rounded()) == classIndex }
        case .double: scan(Double.self) { Int32($0.rounded()) == classIndex }
        default:
            for i in 0..<(w * h) where array[i].int32Value == classIndex {
                hard[i] = 255
                any = true
            }
        }
        guard any else { return nil }
        return VisionMask(width: w, height: h, bytes: soften(hard, width: w, height: h))
    }

    /// 3×3 box average (the window shrinks at the edges). Separable: a row pass, then a
    /// column pass over the row sums, on raw buffers (same results as the direct 9-tap
    /// loop, a fraction of the work).
    nonisolated static func soften(_ m: [UInt8], width w: Int, height h: Int) -> [UInt8] {
        guard w > 0, h > 0, m.count >= w * h else { return m }
        var rows = [Int32](repeating: 0, count: w * h)
        var out = [UInt8](repeating: 0, count: w * h)
        m.withUnsafeBufferPointer { src in
            rows.withUnsafeMutableBufferPointer { r in
                for y in 0..<h {
                    let o = y * w
                    for x in 0..<w {
                        var sum = Int32(src[o + x])
                        if x > 0 { sum += Int32(src[o + x - 1]) }
                        if x + 1 < w { sum += Int32(src[o + x + 1]) }
                        r[o + x] = sum
                    }
                }
            }
            rows.withUnsafeBufferPointer { r in
                out.withUnsafeMutableBufferPointer { dst in
                    for y in 0..<h {
                        let ny: Int32 = (y > 0 ? 1 : 0) + 1 + (y + 1 < h ? 1 : 0)
                        for x in 0..<w {
                            let nx: Int32 = (x > 0 ? 1 : 0) + 1 + (x + 1 < w ? 1 : 0)
                            var sum = r[y * w + x]
                            if y > 0 { sum += r[(y - 1) * w + x] }
                            if y + 1 < h { sum += r[(y + 1) * w + x] }
                            dst[y * w + x] = UInt8(sum / (nx * ny))
                        }
                    }
                }
            }
        }
        return out
    }
}

/// One segmentation off the main actor (MLModel predictions are thread-safe).
private struct SegmentJob: @unchecked Sendable {
    let image: CGImage
    let model: MLModel
    let classIndex: Int32

    func run() throws -> VisionMask? {
        try SkySegmentation.segment(image, model: model, classIndex: classIndex)
    }
}
