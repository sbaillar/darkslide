import CoreGraphics
import CoreImage
import Foundation
import Vision

/// A face on a photo: its box (normalized to the upright photo, origin top
/// left), how good a capture it is, and its identity vector.
public struct FoundFace: Sendable, Equatable {
    public var rect: CGRect
    public var quality: Float
    public var vector: [Float]

    public init(rect: CGRect, quality: Float, vector: [Float]) {
        self.rect = rect
        self.quality = quality
        self.vector = vector
    }
}

/// Finds faces on an image and embeds each one (seam for tests).
public protocol FaceEmbedding: Sendable {
    /// Stored with every face; faces of different models never mix.
    var modelID: String { get }
    func faces(in image: CGImage) throws -> [FoundFace]
}

/// Apple Vision, on device: `VNDetectFaceRectanglesRequest` (with roll),
/// `VNDetectFaceCaptureQualityRequest`, and `VNGenerateImageFeaturePrintRequest`
/// (revision 2, 768 floats) on each face cropped square around its box (1.4×)
/// and rotated upright by its roll.
///
/// The feature print is a general image descriptor, not a face-identity model:
/// it also sees hair, glasses and lighting, so clusters need the user's
/// confirmations (measured on an augmentation proxy only: same face 0.89 mean
/// cosine, two different people 0.59; see `PeopleModel.tuning`).
public struct VisionFaceEmbedder: FaceEmbedding {
    public static let modelID = "vision-featureprint2-face1.4"
    public var modelID: String { Self.modelID }
    /// Faces narrower than this (in the image's pixels) are skipped.
    public var minFacePixels: CGFloat = 40
    public var margin: CGFloat = 1.4
    public static let cropSide: CGFloat = 224

    /// Core Image contexts are thread-safe; one serves every indexer thread.
    private static let context = CIContext(options: [.cacheIntermediates: false])

    public init() {}

    public func faces(in image: CGImage) throws -> [FoundFace] {
        let detect = VNDetectFaceRectanglesRequest()
        detect.revision = VNDetectFaceRectanglesRequestRevision3
        try VNImageRequestHandler(cgImage: image).perform([detect])
        let width = CGFloat(image.width)
        let observed = (detect.results ?? []).filter { $0.boundingBox.width * width >= minFacePixels }
        guard !observed.isEmpty else { return [] }
        let quality = VNDetectFaceCaptureQualityRequest()
        quality.inputFaceObservations = observed
        // Quality is a ranking hint only: without it every face counts as average.
        let rated = (try? VNImageRequestHandler(cgImage: image).perform([quality])).flatMap { quality.results } ?? observed
        return try rated.compactMap { face in
            guard let crop = Self.alignedCrop(image, face: face, margin: margin) else { return nil }
            let b = face.boundingBox
            return FoundFace(rect: CGRect(x: b.minX, y: 1 - b.maxY, width: b.width, height: b.height),
                             quality: face.faceCaptureQuality ?? 0.5,
                             vector: try Self.featurePrint(crop))
        }
    }

    /// The face square around its box (`margin` × the longer side), rotated
    /// upright by its roll, at `cropSide` pixels.
    static func alignedCrop(_ image: CGImage, face: VNFaceObservation, margin: CGFloat) -> CGImage? {
        let w = CGFloat(image.width), h = CGFloat(image.height)
        let b = face.boundingBox // normalized, origin bottom left (as Core Image)
        let rect = CGRect(x: b.minX * w, y: b.minY * h, width: b.width * w, height: b.height * h)
        let side = max(rect.width, rect.height) * margin
        guard side > 0 else { return nil }
        let roll = CGFloat(face.roll?.doubleValue ?? 0)
        let t = CGAffineTransform(translationX: -rect.midX, y: -rect.midY)
            .concatenating(CGAffineTransform(rotationAngle: -roll))
            .concatenating(CGAffineTransform(scaleX: cropSide / side, y: cropSide / side))
            .concatenating(CGAffineTransform(translationX: cropSide / 2, y: cropSide / 2))
        let out = CIImage(cgImage: image).clampedToExtent().transformed(by: t)
            .cropped(to: CGRect(x: 0, y: 0, width: cropSide, height: cropSide))
        return context.createCGImage(out, from: out.extent)
    }

    static func featurePrint(_ crop: CGImage) throws -> [Float] {
        let request = VNGenerateImageFeaturePrintRequest()
        request.revision = VNGenerateImageFeaturePrintRequestRevision2
        request.imageCropAndScaleOption = .scaleFill
        try VNImageRequestHandler(cgImage: crop).perform([request])
        guard let print = request.results?.first else { return [] }
        return floats(print.data, type: print.elementType, count: print.elementCount)
    }

    static func floats(_ data: Data, type: VNElementType, count: Int) -> [Float] {
        data.withUnsafeBytes { p in
            switch type {
            case .double: (0..<count).map { Float(p.load(fromByteOffset: $0 * 8, as: Double.self)) }
            default: (0..<count).map { p.load(fromByteOffset: $0 * 4, as: Float.self) }
            }
        }
    }
}
