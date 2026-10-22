import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

private func meta(_ date: Date) -> PhotoMeta {
    PhotoMeta(camera: "Sony ILCE-9", lens: "FE 85mm F1.4 GM", iso: 100, shutter: 0.004, aperture: 2, focalLength: 85,
              captureDate: date, pixelWidth: 6000, pixelHeight: 4000, fileSize: 1)
}

private func photos(_ n: Int, spacing: TimeInterval = 60) -> [Photo] {
    (0..<n).map { Photo(id: PhotoID(Int64($0)), filename: "P\($0).ARW", meta: meta(Date(timeIntervalSince1970: Double($0) * spacing))) }
}

private func solidImage(r: CGFloat, g: CGFloat, b: CGFloat) -> CGImage {
    let ctx = CGContext(data: nil, width: 64, height: 64, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
    ctx.setFillColor(CGColor(srgbRed: r, green: g, blue: b, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: 64, height: 64))
    return ctx.makeImage()!
}

/// Serves a fixed image (or an error) for every photo.
private struct FixedImages: ImageProvider {
    var image: CGImage?
    func thumbnail(for photo: Photo, maxPixelSize: Int) async throws -> CGImage {
        guard let image else { throw MockError.notFound("thumbnail") }
        return image
    }
}

private final class ProgressLog: @unchecked Sendable {
    private let lock = NSLock()
    private var calls: [(Int, Int)] = []
    func add(_ d: Int, _ t: Int) { lock.withLock { calls.append((d, t)) } }
    var last: (Int, Int)? { lock.withLock { calls.last } }
    var count: Int { lock.withLock { calls.count } }
}

@Suite struct AutoAdjustTests {
    @Test func grayWorldWhiteBalance() {
        let neutral = AutoAdjust.whiteBalance(meanLinearRGB: SIMD3(0.2, 0.2, 0.2))
        #expect(neutral.temperature == 5500 && neutral.tint == 0)
        #expect(AutoAdjust.whiteBalance(meanLinearRGB: SIMD3(0.1, 0.2, 0.3)).temperature > 5500, "blue cast → warm up")
        #expect(AutoAdjust.whiteBalance(meanLinearRGB: SIMD3(0.3, 0.2, 0.1)).temperature < 5500, "orange cast → cool down")
        #expect(AutoAdjust.whiteBalance(meanLinearRGB: SIMD3(0.2, 0.3, 0.2)).tint > 0, "green cast → magenta")
        let extreme = AutoAdjust.whiteBalance(meanLinearRGB: SIMD3(0, 1, 1))
        #expect(extreme.temperature == 12_000 && extreme.tint == 50)
        #expect(AutoAdjust.whiteBalance(meanLinearRGB: SIMD3(1, 0, 0)).tint == -50)
    }

    @Test func autoTone() {
        #expect(AutoAdjust.tone(meanLuma: 0.46, highlightClip: 0, shadowClip: 0) == (0, 0, 0))
        let dark = AutoAdjust.tone(meanLuma: 0.2, highlightClip: 0, shadowClip: 0.05)
        #expect(dark.ev > 0.5 && dark.shadows == 20 && dark.highlights == 0)
        let bright = AutoAdjust.tone(meanLuma: 0.8, highlightClip: 0.02, shadowClip: 0)
        #expect(bright.ev < 0 && bright.highlights == 30, "positive recovers highlights (ds-core convention)")
        #expect(AutoAdjust.tone(meanLuma: 0, highlightClip: 1, shadowClip: 1) == (2, 50, 60))
    }

    /// Auto sets WB and tone and never straightens (coordinator: straighten is explicit only).
    @Test func applySetsWBAndToneButNeverStraightens() {
        let stats = ImageStatistics(width: 1, height: 1, meanLinearRGB: SIMD3(0.1, 0.2, 0.3), meanLuma: 0.3,
                                    highlightClip: 0, shadowClip: 0, laplacianVariance: 0)
        let e = AutoAdjust.apply(statistics: stats, to: EditState())
        #expect(!e.whiteBalance.asShot && e.whiteBalance.temperature > 5500)
        #expect(e.exposure.ev > 0)
        #expect(e.crop.angle == 0)
        var tilted = EditState()
        tilted.crop.angle = 1.7
        #expect(AutoAdjust.apply(statistics: stats, to: tilted).crop.angle == 1.7, "an existing angle is kept")
    }

    /// The live service's Auto leaves `crop.angle` alone even on a clearly tilted image.
    @Test func serviceAutoNeverStraightens() async throws {
        let e = try await VisionAIService(language: RulesEditService())
            .autoAdjust(samplePhoto(1), current: EditState(), images: MockImageProvider(latency: .zero))
        #expect(e.crop.angle == 0)
    }
}

@Suite struct EditIntentMappingTests {
    @Test func warmthStartsFromNeutralWhenAsShot() {
        let e = EditState().applying(EditIntent(temperatureShift: 800, tintShift: 5))
        #expect(!e.whiteBalance.asShot && e.whiteBalance.temperature == 6300 && e.whiteBalance.tint == 5)
        var custom = EditState()
        custom.whiteBalance.asShot = false
        custom.whiteBalance.temperature = 3000
        #expect(custom.applying(EditIntent(temperatureShift: -3000)).whiteBalance.temperature == 2000, "clamped")
    }

    @Test func deltasAreClamped() {
        var start = EditState()
        start.exposure.ev = 4.5
        start.hdr.shadows = 90
        let e = start.applying(EditIntent(exposureShift: 1, contrastShift: 30, highlightsShift: -20, shadowsShift: 30, saturationShift: -100))
        #expect(e.exposure.ev == 5 && e.hdr.shadows == 100)
        #expect(e.exposure.contrast == 30 && e.hdr.highlights == -20 && e.exposure.saturation == -100)
        #expect(e.whiteBalance.asShot, "untouched WB stays as shot")
    }

    @Test func warmthTargetBandsAndDegrees() {
        var i = EditIntent()
        i.warmth = 25 // 15 mired warmer
        let warm = EditState().applying(i)
        #expect(abs(warm.whiteBalance.temperature - (1_000_000 / (1_000_000 / 5500.0 - 15)).rounded()) < 1e-9)
        var target = EditIntent(temperatureShift: 100)
        target.temperatureTarget = 3200
        #expect(EditState().applying(target).whiteBalance.temperature == 3300, "target first, then shifts")
        var extreme = EditIntent()
        extreme.warmth = 100_000
        #expect(EditState().applying(extreme).whiteBalance.temperature == 50_000, "mired floor + clamp")
        var bands = EditIntent()
        bands.bandShifts = [BandShift(band: 5, saturation: 30, lightness: -150), BandShift(band: 12, saturation: 1, lightness: 1)]
        let b = EditState().applying(bands)
        #expect(b.colorEditor.bands[5].saturation == 30 && b.colorEditor.bands[5].lightness == -100)
        var deg = EditIntent()
        deg.straightenDegrees = 50
        #expect(EditState().applying(deg).crop.angle == 45)
    }

    @Test func cropAndStraighten() {
        #expect(EditState().applying(EditIntent(cropAspect: "4:5")).crop.aspect == .r4x5)
        #expect(EditState().applying(EditIntent(cropAspect: "7:3")).crop.aspect == .original)
        #expect(EditState().applying(EditIntent(straighten: true)).crop.angle == 0, "no horizon → no change")
        #expect(EditState().applying(EditIntent(straighten: true), horizonDegrees: 2.5).crop.angle == -2.5)
    }
}

@Suite struct VisionAIServiceTests {
    let service = VisionAIService(language: RulesEditService(), maxConcurrent: 2)

    @Test func cullsWithDSAIAndVisionAndReportsProgress() async throws {
        let ps = photos(5)
        let log = ProgressLog()
        let out = try await service.cull(ps, images: MockImageProvider(latency: .zero)) { log.add($0, $1) }
        #expect(out.map(\.photoID) == ps.map(\.id))
        #expect(out.allSatisfy { $0.score >= 0 && $0.score <= 1 && !$0.reasons.contains("Could not analyze") })
        #expect(log.count >= 1, "progress per ds-ai batch [nl9]")
        #expect(log.last! == (5, 5))
    }

    @Test func unreadableImagesStayUndecided() async throws {
        let out = try await service.cull(photos(2), images: FixedImages(image: nil)) { _, _ in }
        #expect(out.allSatisfy { $0.pick == .none && $0.reasons == ["Could not analyze"] })
    }

    @Test func cullHonoursCancellation() async {
        let svc = service
        let task = Task {
            try await svc.cull(photos(200), images: MockImageProvider(latency: .milliseconds(20))) { _, _ in }
        }
        task.cancel()
        await #expect(throws: CancellationError.self) { try await task.value }
    }

    @Test func autoAdjustWarmsABlueImage() async throws {
        let p = photos(1)[0]
        let e = try await service.autoAdjust(p, current: EditState(), images: FixedImages(image: solidImage(r: 0.3, g: 0.45, b: 0.8)))
        #expect(!e.whiteBalance.asShot && e.whiteBalance.temperature > 5500)
    }

    @Test func instructionGoesThroughDSAIRules() async throws {
        let e = try await service.applyInstruction("warmer and +0.5 ev, crop 4:5", to: EditState())
        #expect(!e.whiteBalance.asShot && e.whiteBalance.temperature > 5500)
        #expect(e.exposure.ev == 0.5 && e.crop.aspect == .r4x5)
    }

    @Test func keywordsAreHumanReadable() async throws {
        let words = try await service.keywords(for: photos(1)[0], images: MockImageProvider(latency: .zero), limit: 3)
        #expect(words.count <= 3)
        #expect(words.allSatisfy { !$0.contains("_") })
    }

    /// [nl9] The factory's language service is the guarded pipeline with the rules first.
    @MainActor @Test func factoryUsesVisionWithTheGuardedParser() {
        let s = makeAIService(defaults: UserDefaults(suiteName: testSuiteName())!) as? VisionAIService
        #expect(s != nil)
        #expect((s?.language as? GuardedEditService)?.rules is RulesEditService)
        #expect((s?.language as? GuardedEditService)?.models.first is JevEditService)
    }
}
