import CoreGraphics
import CoreML
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import DarkslideKit

extension DenoiseGlobalStore {
    /// AI Denoise end to end: the real model (`$DS_DENOISE_MODEL_DIR`: the package
    /// `scripts/denoise-model/build-model.sh` prints, or the one inside a built app), Core ML on the GPU and Neural Engine, the Rust tiles, cache
    /// and pipeline, on the real high-ISO sample. Skips without the model or the samples.
    /// `$DS_DENOISE_CROPS_DIR` also writes 1:1 crops (original, existing NR at its max, AI
    /// Denoise at 50 and 100) for a visual check.
    @MainActor
    @Suite(.serialized) struct DenoiseLiveTests {
        /// The package, or a built `Darkslide.app` (found the way the app finds it).
        static var package: URL? {
            guard let path = ProcessInfo.processInfo.environment["DS_DENOISE_MODEL_DIR"] else { return nil }
            let url = URL(fileURLWithPath: path)
            guard url.pathExtension == "app" else { return url }
            return Bundle(url: url).flatMap(DenoiseModelDescriptor.bundledPackage(in:))
        }

        static func sample(_ name: String) -> URL? { sampleARWs().first { $0.lastPathComponent == name } }

        func service(_ root: URL) -> DenoiseService {
            DenoiseService(
                model: DenoiseModelStore(package: Self.package!, compiledRoot: root.appendingPathComponent("Models")),
                backend: RustDenoiseBackend(),
                cacheDir: root.appendingPathComponent("DenoiseCache"))
        }

        static func crop(_ photo: Photo, _ edit: EditState, _ rect: CGRect) async throws -> CGImage {
            let f = try await DevelopRenderer().render(photo, edit: edit, request: RenderRequest(
                maxPixelSize: 6000, scale: 1.0, region: rect, quality: .full, geometry: .final))
            var px = [UInt8](repeating: 0, count: f.width * f.height * 4)
            f.withBytes { raw in
                for y in 0..<f.height {
                    for i in 0..<(f.width * 4) { px[y * f.width * 4 + i] = raw[y * f.bytesPerRow + i] }
                }
            }
            let ctx = CGContext(data: &px, width: f.width, height: f.height, bitsPerComponent: 8, bytesPerRow: f.width * 4,
                                space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
            return ctx.makeImage()!
        }

        static func write(_ img: CGImage, _ url: URL) {
            guard let d = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else { return }
            CGImageDestinationAddImage(d, img, nil)
            CGImageDestinationFinalize(d)
        }

        /// Peak resident memory of this process (bytes).
        static func peakRSS() -> Int {
            var u = rusage()
            getrusage(RUSAGE_SELF, &u)
            return u.ru_maxrss
        }

        @Test func denoisesTheHighISOSampleWithinBudgetAndRendersIt() async throws {
            guard Self.package != nil, let url = Self.sample("SKB06204.ARW"), let second = Self.sample("SKB06368.ARW") else {
                print("AI Denoise live: no model package or samples, skipped")
                return
            }
            let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-denoise-live-\(UUID())")
            defer { try? FileManager.default.removeItem(at: root) }
            let s = service(root)
            let rssBefore = Self.peakRSS()

            // First use: verify the pinned checksum, compile, load both instances, denoise.
            let clock = ContinuousClock()
            var t = clock.now
            try await s.denoise(url)
            let first = clock.now - t
            #expect(s.states[url] == .ready)

            // A photo with the model loaded: the per-photo cost (decode, demosaic, 117 tiles on
            // the GPU and Neural Engine, stitch, cache write).
            t = clock.now
            try await s.denoise(second)
            let warm = clock.now - t

            // The viewer keeps rendering while a photo is denoised: preview renders of another
            // photo, timed, during a third denoise.
            let photo = try await RustRaw.photo(at: url, id: PhotoID(1))
            let other = try await RustRaw.photo(at: second, id: PhotoID(2))
            let renderer = DevelopRenderer()
            var lat: [Duration] = []
            let third = Self.sample("SKB06426.ARW") ?? url
            let job = Task { try await s.denoise(third) }
            repeat {
                let r0 = clock.now
                var e = EditState()
                e.exposure.ev = Double(lat.count % 7) * 0.1 // a slider moving: develop work every frame
                _ = try await renderer.render(other, edit: e, request: RenderRequest(maxPixelSize: 2560, quality: .draft))
                lat.append(clock.now - r0)
            } while (s.states[third] != .ready && lat.count < 300) || lat.count < 20
            _ = try await job.value
            let sorted = lat.sorted()
            let p50 = sorted[sorted.count / 2], p90 = sorted[sorted.count * 9 / 10], worst = sorted.last!
            let rss = Self.peakRSS()
            print(String(format: "AI Denoise live: first use %.2f s (verify + compile + load + denoise), per photo %.2f s; "
                         + "viewer drafts during denoise p50 %.0f ms p90 %.0f ms max %.0f ms (%d); peak RSS %d MB (+%d MB)",
                         first.seconds, warm.seconds, p50.ms, p90.ms, worst.ms, lat.count, rss >> 20, (rss - rssBefore) >> 20))
            #expect(warm < .seconds(5), "per-photo budget on the M4 Pro")

            // Renders use the denoised image; strength 0 is the original.
            var ai = EditState()
            ai.noise.aiDenoise = true
            ai.noise.aiStrength = 100
            let region = CGRect(x: 0.41, y: 0.465, width: 0.08, height: 0.054)
            let original = try await Self.crop(photo, EditState(), region)
            let denoised = try await Self.crop(photo, ai, region)
            #expect(original.width == denoised.width)
            var zero = ai
            zero.noise.aiStrength = 0
            let zeroCrop = try await Self.crop(photo, zero, region)
            #expect(zeroCrop.dataProvider?.data == original.dataProvider?.data, "strength 0 is bit-identical")
            #expect(denoised.dataProvider?.data != original.dataProvider?.data)

            if let dir = ProcessInfo.processInfo.environment["DS_DENOISE_CROPS_DIR"] {
                let out = URL(fileURLWithPath: dir)
                try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
                var nrMax = EditState()
                nrMax.noise.luminance = 100
                nrMax.noise.color = 100
                var half = ai
                half.noise.aiStrength = 50
                let regions = [("people", region), ("rock", CGRect(x: 0.55, y: 0.60, width: 0.16, height: 0.107)),
                               ("shadow", CGRect(x: 0.02, y: 0.75, width: 0.16, height: 0.107)),
                               ("water", CGRect(x: 0.25, y: 0.90, width: 0.16, height: 0.08))]
                for (name, r) in regions {
                    for (label, e) in [("original", EditState()), ("nr-max", nrMax), ("ai-50", half), ("ai-100", ai)] {
                        Self.write(try await Self.crop(photo, e, r), out.appendingPathComponent("\(name)-\(label).png"))
                    }
                }
            }
        }
    }
}

private extension Duration {
    var seconds: Double { Double(components.seconds) + Double(components.attoseconds) / 1e18 }
    var ms: Double { seconds * 1000 }
}
