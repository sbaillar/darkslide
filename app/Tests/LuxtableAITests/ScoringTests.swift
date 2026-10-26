import CoreGraphics
import Foundation
import Testing
@testable import LuxtableAI

@Suite struct ImageStatisticsTests {
    @Test func uniformGrayHasNoEdgesAndMidLuma() throws {
        let s = try #require(ImageStatistics(image: TestImages.solid(64, 48, gray: 0.5)))
        #expect(s.width == 64 && s.height == 48)
        #expect(s.laplacianVariance == 0)
        #expect(abs(s.meanLuma - 0.5) < 0.02)
        #expect(s.highlightClip == 0 && s.shadowClip == 0)
        // Linear mean of sRGB 0.5 ≈ 0.214, equal in all channels.
        #expect(abs(s.meanLinearRGB.x - 0.214) < 0.01)
        #expect(abs(s.meanLinearRGB.x - s.meanLinearRGB.z) < 1e-9)
    }

    @Test func clippingIsCounted() throws {
        let white = try #require(ImageStatistics(image: TestImages.solid(10, 10, gray: 1)))
        #expect(white.highlightClip == 1 && white.shadowClip == 0)
        let black = try #require(ImageStatistics(image: TestImages.solid(10, 10, gray: 0)))
        #expect(black.shadowClip == 1 && black.highlightClip == 0)
    }

    @Test func edgesRaiseLaplacianVariance() throws {
        let sharp = try #require(ImageStatistics(image: TestImages.checkerboard(128, 128)))
        let soft = try #require(ImageStatistics(image: TestImages.gradient(128, 128)))
        #expect(sharp.laplacianVariance > 10_000)
        #expect(soft.laplacianVariance < 5)
    }

    @Test func downsamplesToMaxEdge() throws {
        let s = try #require(ImageStatistics(image: TestImages.solid(1000, 500, gray: 0.3), maxEdge: 100))
        #expect(s.width == 100 && s.height == 50)
    }

    @Test func tinyImagesHaveZeroVariance() {
        #expect(ImageStatistics.laplacianVariance([1, 2, 3, 4], width: 2, height: 2) == 0)
        let s = ImageStatistics(rgba: [UInt8](repeating: 200, count: 4), width: 1, height: 1)
        #expect(s.laplacianVariance == 0)
        #expect(s.width == 1)
    }
}

@Suite struct EyeMetricsTests {
    /// Points on an ellipse with the given half-axes, rotated by `angle`.
    func eye(_ a: Double, _ b: Double, angle: Double = 0) -> [CGPoint] {
        (0..<8).map { i in
            let t = Double(i) / 8 * 2 * .pi
            let (x, y) = (a * cos(t), b * sin(t))
            return CGPoint(x: x * cos(angle) - y * sin(angle) + 0.5, y: x * sin(angle) + y * cos(angle) + 0.5)
        }
    }

    @Test func openAndClosedRatios() throws {
        let open = try #require(EyeMetrics.aspectRatio(eye(0.1, 0.035)))
        let closed = try #require(EyeMetrics.aspectRatio(eye(0.1, 0.005)))
        #expect(abs(open - 0.35) < 0.02)
        #expect(closed < 0.06)
    }

    @Test func tiltDoesNotChangeTheRatio() throws {
        let flat = try #require(EyeMetrics.aspectRatio(eye(0.1, 0.03)))
        let tilted = try #require(EyeMetrics.aspectRatio(eye(0.1, 0.03, angle: 0.5)))
        #expect(abs(flat - tilted) < 0.01)
    }

    @Test func degenerateInputs() {
        #expect(EyeMetrics.aspectRatio([.zero, CGPoint(x: 1, y: 1)]) == nil)
        #expect(EyeMetrics.aspectRatio([CGPoint(x: 0.2, y: 0.2), CGPoint(x: 0.2, y: 0.2), CGPoint(x: 0.2, y: 0.2)]) == nil)
    }

    @Test func openProbabilityRamps() {
        #expect(EyeMetrics.openProbability(left: 0.3, right: 0.3) == 1)
        #expect(EyeMetrics.openProbability(left: 0.05, right: 0.1) == 0)
        #expect(abs(EyeMetrics.openProbability(left: 0.18, right: 0.18)! - 0.5) < 1e-9)
        #expect(EyeMetrics.openProbability(left: nil, right: 0.3) == nil)
        #expect(EyeMetrics.openProbability(left: 0.3, right: 0.3, closedRatio: 0.3, openRatio: 0.2) == nil)
    }

    @Test func eyesOpenNeedsBothEyes() {
        #expect(EyeMetrics.eyesOpen(left: 0.3, right: 0.3) == true)
        #expect(EyeMetrics.eyesOpen(left: 0.05, right: 0.08) == false)
        #expect(EyeMetrics.eyesOpen(left: nil, right: 0.3) == nil)
        let face = FaceInfo(boundingBox: .zero, captureQuality: 0.5, leftEyeAspect: 0.3, rightEyeAspect: 0.25)
        #expect(face.eyesOpen == true)
    }
}

@Suite struct SmartCropTests {
    let landscape = CGSize(width: 6000, height: 4000)

    @Test func centersWithoutSaliency() {
        let r = SmartCrop.suggest(aspect: 1, imageSize: landscape, salient: [])
        #expect(abs(r.width - 4000.0 / 6000) < 1e-9 && r.height == 1)
        #expect(abs(r.midX - 0.5) < 1e-9)
    }

    @Test func followsSubjectAndStaysInside() {
        let right = SmartCrop.suggest(aspect: 4.0 / 5, imageSize: landscape, salient: [CGRect(x: 0.85, y: 0.4, width: 0.1, height: 0.2)])
        #expect(right.maxX <= 1 + 1e-9)
        #expect(abs(right.maxX - 1) < 1e-9, "clamped to the right edge")
        let mid = SmartCrop.suggest(aspect: 4.0 / 5, imageSize: landscape,
                                    salient: [CGRect(x: 0.35, y: 0.4, width: 0.1, height: 0.1), CGRect(x: 0.45, y: 0.5, width: 0.1, height: 0.1)])
        #expect(abs(mid.midX - 0.45) < 1e-9)
    }

    @Test func widerThanImageUsesFullWidth() {
        let r = SmartCrop.suggest(aspect: 16.0 / 9, imageSize: landscape, salient: [CGRect(x: 0.4, y: 0.0, width: 0.1, height: 0.05)])
        #expect(r.width == 1)
        #expect(abs(r.height - 1.5 / (16.0 / 9)) < 1e-9)
        #expect(r.minY == 0)
    }

    @Test func invalidInputReturnsFullFrame() {
        #expect(SmartCrop.suggest(aspect: 0, imageSize: landscape, salient: []) == CGRect(x: 0, y: 0, width: 1, height: 1))
    }

    @Test func parsesAspects() {
        #expect(SmartCrop.parseAspect("4:5") == 0.8)
        #expect(SmartCrop.parseAspect("16x9") == 16.0 / 9)
        #expect(SmartCrop.parseAspect("1/1") == 1)
        #expect(SmartCrop.parseAspect("wide") == nil)
        #expect(SmartCrop.parseAspect("0:5") == nil)
    }
}
