import DarkslideFFI
import Foundation
import Metal

/// `Darkslide --self-test`: proves the Rust → FFI → Swift → Metal path headlessly.
///
/// 1. The linked Rust library's FFI contract matches these bindings.
/// 2. The Rust `Engine` renders straight into a Metal buffer, with progress events.
/// 3. Known test-pattern pixels have the expected sRGB values.
/// 4. A 16-bit float linear-P3 frame has the expected values (EDR path).
/// 5. Cancelling a large render returns early (Swift task → Rust token).
/// 6. The frame uploads to a Metal texture and the runtime-compiled viewer
///    shader draws it; the read-back pixel matches the source.
/// 7. Swift's copy/apply group table matches Rust's.
public enum SelfTest {
    public struct Check: Sendable {
        public let name: String
        public let passed: Bool
        public let detail: String
    }

    @MainActor
    public static func run(log: (String) -> Void = { print($0) }) async -> [Check] {
        var checks: [Check] = []
        func check(_ name: String, _ ok: Bool, _ detail: String) {
            checks.append(Check(name: name, passed: ok, detail: detail))
            log("\(ok ? "PASS" : "FAIL")  \(name): \(detail)")
        }

        check("ffi contract", RustCore.contract == expectedFFIContract,
              "rust \(RustCore.version), swift expects \(expectedFFIContract)")

        let events = CountingProgress()
        let engine: Engine
        do {
            engine = try Engine()
            try engine.setProgressListener(listener: events)
        } catch {
            check("rust engine", false, "\(error)")
            return checks
        }
        defer { try? engine.setProgressListener(listener: nil) }
        let renderer = RustRenderer(engine: engine)
        let photo = selfTestPhoto(width: 900, height: 600)
        let clock = ContinuousClock()
        let frame: RenderedFrame
        do {
            let start = clock.now
            frame = try await renderer.render(photo, edit: EditState(), maxPixelSize: 900)
            let ms = (clock.now - start).inMilliseconds
            _ = try engine.flushProgress()
            let inBuffer = if case .buffer = frame.backing { true } else { false }
            check("rust render", frame.width == 900 && frame.height == 600 && inBuffer,
                  "\(frame.width)x\(frame.height) in \(String(format: "%.1f", ms)) ms into "
                      + (inBuffer ? "an MTLBuffer (0 copies)" : "heap memory"))
        } catch {
            check("rust render", false, "\(error)")
            return checks
        }
        check("progress events", events.value >= 3, "\(events.value) delivered")

        // White chart patch (row 3, col 0) and the red patch (row 2, col 2).
        let white = pixel(frame, x: Int(0.5 / 6 * 900), y: Int((0.45 + 3.5 / 4 * 0.40) * 600))
        let red = pixel(frame, x: Int(2.5 / 6 * 900), y: Int((0.45 + 2.5 / 4 * 0.40) * 600))
        check("pixel values", white == [243, 243, 242, 255] && red == [175, 54, 60, 255],
              "white \(white), red \(red)")

        do {
            let wide = selfTestPhoto(width: 1100, height: 100)
            let f16 = try await renderer.render(wide, edit: EditState(),
                                                request: RenderRequest(maxPixelSize: 1100, format: .rgba16fLinearP3))
            let top = halfPixel(f16, x: 1095, y: 98)
            check("16F linear P3", f16.format == .rgba16fLinearP3 && abs(top[0] - 1) < 0.01 && abs(top[3] - 1) < 0.001,
                  "wedge white = \(top.map { String(format: "%.3f", $0) }.joined(separator: ", "))")
        } catch {
            check("16F linear P3", false, "\(error)")
        }

        let cancel = await measureCancel(renderer)
        // Relative bound: under heavy load a full render slows too; cancel must beat it clearly.
        check("cancel", cancel.cancelled && cancel.afterCancelMs < max(100, cancel.fullMs * 0.5),
              String(format: "full 6000x4000 render %.1f ms; cancelled after 2 ms, returned %.1f ms later",
                     cancel.fullMs, cancel.afterCancelMs))

        do {
            let metal = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
            guard let tex = metal.makeTexture(from: frame) else {
                check("metal viewer", false, "texture upload failed")
                return checks
            }
            let (w, h) = (300, 200)
            let pane = MetalViewerRenderer.Pane(after: tex, rect: SIMD4(-1, -1, 1, 1))
            guard let out = metal.renderOffscreen([pane], width: w, height: h, clear: .init(red: 0, green: 0, blue: 0, alpha: 1)) else {
                check("metal viewer", false, "offscreen render failed")
                return checks
            }
            let gx = Int(0.5 / 6 * Double(w)), gy = Int((0.45 + 3.5 / 4 * 0.40) * Double(h))
            let i = (gy * w + gx) * 4
            let got = Array(out[i..<i + 4])
            let close = zip(got, white).allSatisfy { abs(Int($0) - Int($1)) <= 2 }
            check("metal viewer", close, "\(metal.device.name): drew \(w)x\(h), white patch \(got)")
        } catch {
            check("metal viewer", false, "\(error)")
        }

        let rust = RustCore.groups
        let matches = rust.map(\.0) == AdjustGroup.allCases && rust.allSatisfy { $0.0.supportsRelative == $0.1 }
        check("adjust groups", matches, "\(rust.count) groups agree with ds-core")
        return checks
    }

    struct CancelMeasurement {
        var cancelled: Bool
        var fullMs: Double
        var afterCancelMs: Double
    }

    /// Time a full 1:1 6000×4000 render, then start another and cancel it after 2 ms.
    static func measureCancel(_ renderer: RustRenderer) async -> CancelMeasurement {
        let photo = selfTestPhoto(width: 6000, height: 4000)
        let request = RenderRequest(maxPixelSize: 6000, scale: 1)
        let clock = ContinuousClock()
        let t0 = clock.now
        _ = try? await renderer.render(photo, edit: EditState(), request: request)
        let full = (clock.now - t0).inMilliseconds
        let task = Task { try await renderer.render(photo, edit: EditState(), request: request) }
        try? await Task.sleep(for: .milliseconds(2))
        let t1 = clock.now
        task.cancel()
        let result = await task.result
        let after = (clock.now - t1).inMilliseconds
        var cancelled = false
        if case .failure(let e) = result, e is CancellationError { cancelled = true }
        return CancelMeasurement(cancelled: cancelled, fullMs: full, afterCancelMs: after)
    }

    static func selfTestPhoto(width: Int, height: Int) -> Photo {
        Photo(id: PhotoID(0), filename: "self-test", meta: PhotoMeta(
            camera: "", lens: "", iso: 100, shutter: 0.01, aperture: 4, focalLength: 50,
            captureDate: Date(), pixelWidth: width, pixelHeight: height, fileSize: 0))
    }

    static func pixel(_ f: RenderedFrame, x: Int, y: Int) -> [UInt8] {
        f.withBytes { p in
            let i = y * f.bytesPerRow + x * 4
            return [p[i], p[i + 1], p[i + 2], p[i + 3]]
        }
    }

    /// RGBA of a 16F frame pixel as `Float`.
    static func halfPixel(_ f: RenderedFrame, x: Int, y: Int) -> [Float] {
        f.withBytes { p in
            let i = y * f.bytesPerRow + x * 8
            return (0..<4).map { c in
                Float(Float16(bitPattern: UInt16(p[i + 2 * c]) | UInt16(p[i + 2 * c + 1]) << 8))
            }
        }
    }
}

/// Counts progress events (delivered on the engine's delivery thread).
final class CountingProgress: ProgressListener, @unchecked Sendable {
    private let counter = EventCounter()
    func onProgress(event: ProgressEvent) { counter.increment() }
    var value: Int { counter.value }
}

/// Thread-safe counter for events delivered on Rust threads.
final class EventCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    func increment() { lock.withLock { count += 1 } }
    var value: Int { lock.withLock { count } }
}

extension Duration {
    var inMilliseconds: Double {
        let (s, atto) = components
        return Double(s) * 1000 + Double(atto) / 1e15
    }
}
