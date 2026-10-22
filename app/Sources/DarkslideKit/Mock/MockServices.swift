import CoreGraphics
import Foundation

/// Deterministic stand-in for the AI service (no Vision), used in tests and
/// when Vision is unavailable. The app uses `VisionAIService`.
public struct MockAIService: AIService {
    public init() {}

    public func cull(_ photos: [Photo], images: any ImageProvider,
                     progress: @escaping @Sendable (Int, Int) -> Void) async throws -> [CullSuggestion] {
        var out: [CullSuggestion] = []
        for (i, p) in photos.enumerated() {
            try Task.checkCancellation()
            var rng = SplitMix64(seed: UInt64(p.id.raw))
            let score = rng.unit()
            let pick: PickState = score > 0.8 ? .picked : score < 0.15 ? .rejected : .none
            out.append(CullSuggestion(photoID: p.id, score: score, pick: pick,
                                      reasons: [pick == .rejected ? "Soft focus" : "Sharp subject"]))
            progress(i + 1, photos.count)
        }
        return out
    }

    public func autoAdjust(_ photo: Photo, current: EditState, images: any ImageProvider) async throws -> EditState {
        var e = current
        e.whiteBalance.asShot = false
        e.whiteBalance.temperature = 5200
        e.exposure.ev = 0.3
        e.hdr.shadows = 20
        e.hdr.highlights = -15
        return e
    }

    public func applyInstruction(_ text: String, to current: EditState) async throws -> EditState { current }
}
