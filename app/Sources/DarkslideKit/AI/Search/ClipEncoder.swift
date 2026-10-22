import CoreGraphics
import CoreML
import DarkslideFFI
import Foundation
import Vision

/// Turns photos and query text into comparable embeddings (seam for tests).
public protocol SearchEncoding: Sendable {
    /// Stored with every vector; embeddings of different models never mix.
    var modelID: String { get }
    /// One embedding per image (any length the model produces; Rust normalizes).
    func imageEmbeddings(_ images: [CGImage]) throws -> [[Float]]
    func textEmbedding(_ text: String) throws -> [Float]
}

/// The MobileCLIP encoders on Core ML, with Rust's CLIP tokenizer.
///
/// Both models run on the Neural Engine (with the CPU), never the GPU, so
/// indexing in the background doesn't compete with the viewer's renders. Core ML
/// predictions are thread-safe, so one encoder serves the indexer and queries.
public final class ClipEncoder: SearchEncoding, @unchecked Sendable {
    public let modelID: String
    private let image: MLModel
    private let text: MLModel
    private let tokenizer: ClipTokenizer
    private let imageInput: String
    private let imageConstraint: MLImageConstraint
    private let textInput: String

    /// Loads the installed model. Throws `ClipModelError.load` when a file is
    /// missing or damaged (the caller discards and downloads it again).
    public init(files: ClipModelFiles, modelID: String = ClipModelDescriptor.mobileCLIPS2.id) throws {
        self.modelID = modelID
        let config = MLModelConfiguration()
        config.computeUnits = .cpuAndNeuralEngine
        do {
            image = try MLModel(contentsOf: files.imageModel, configuration: config)
            text = try MLModel(contentsOf: files.textModel, configuration: config)
            tokenizer = try ClipTokenizer.fromMergesFile(path: files.merges.path)
        } catch {
            throw ClipModelError.load(error.localizedDescription)
        }
        guard let (name, desc) = image.modelDescription.inputDescriptionsByName.first(where: { $0.value.type == .image }),
              let constraint = desc.imageConstraint,
              let textName = text.modelDescription.inputDescriptionsByName.first(where: { $0.value.type == .multiArray })?.key
        else { throw ClipModelError.load("unexpected model inputs") }
        imageInput = name
        imageConstraint = constraint
        textInput = textName
    }

    public func imageEmbeddings(_ images: [CGImage]) throws -> [[Float]] {
        guard !images.isEmpty else { return [] }
        // Center crop like CLIP's preprocessing (the short side fills 256 px).
        let options: [MLFeatureValue.ImageOption: Any] = [
            .cropAndScale: VNImageCropAndScaleOption.centerCrop.rawValue,
        ]
        let inputs: [MLFeatureProvider] = try images.map { cg in
            let value = try MLFeatureValue(cgImage: cg, constraint: imageConstraint, options: options)
            return try MLDictionaryFeatureProvider(dictionary: [imageInput: value])
        }
        let out = try image.predictions(fromBatch: MLArrayBatchProvider(array: inputs))
        return try (0..<out.count).map { try Self.vector(out.features(at: $0)) }
    }

    public func textEmbedding(_ query: String) throws -> [Float] {
        let ids = try tokenizer.encode(text: query)
        let array = try MLMultiArray(shape: [1, NSNumber(value: ids.count)], dataType: .int32)
        let p = array.dataPointer.bindMemory(to: Int32.self, capacity: ids.count)
        for (i, id) in ids.enumerated() { p[i] = id }
        let out = try text.prediction(from: MLDictionaryFeatureProvider(dictionary: [textInput: array]))
        return try Self.vector(out)
    }

    /// The model's single multi-array output as floats.
    static func vector(_ features: MLFeatureProvider) throws -> [Float] {
        guard let name = features.featureNames.first, let a = features.featureValue(for: name)?.multiArrayValue else {
            throw ClipModelError.load("the model returned no embedding")
        }
        return floats(a)
    }

    static func floats(_ a: MLMultiArray) -> [Float] {
        let n = a.count
        switch a.dataType {
        case .float32:
            let p = a.dataPointer.bindMemory(to: Float.self, capacity: n)
            return Array(UnsafeBufferPointer(start: p, count: n))
        case .float16:
            let p = a.dataPointer.bindMemory(to: Float16.self, capacity: n)
            return (0..<n).map { Float(p[$0]) }
        default:
            return (0..<n).map { a[$0].floatValue }
        }
    }
}
