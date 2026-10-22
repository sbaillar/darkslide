import AppKit
import CoreGraphics
import Foundation
import ImageIO
import SwiftUI
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Before/after measurements for the GPU/accelerator work. Prints `GPUMEASURE …`
/// lines; bounds are loose (they catch regressions, the printed numbers are the report).
@MainActor
@Suite(.serialized) struct GpuMeasureTests {
    static func ms(_ d: Duration) -> Double { Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15 }

    /// Per-image embedded-preview decode on the real samples at 256 / 1024 / full,
    /// through the production provider (fresh, so nothing is cached).
    @Test func previewDecodePerImage() async throws {
        let files = sampleARWs()
        guard !files.isEmpty else { return }
        let clock = ContinuousClock()
        for size in [256, 1024, 0] {
            let p = RustPreviewProvider()
            _ = try await p.preview(for: files[0], maxPixelSize: size) // warm FFI + ImageIO
            let p2 = RustPreviewProvider()
            var total = Duration.zero
            for f in files {
                let t = clock.now
                let img = try await p2.preview(for: f, maxPixelSize: size)
                total += clock.now - t
                if size > 0 { #expect(max(img.width, img.height) <= size) }
            }
            print(String(format: "GPUMEASURE preview decode @%@: %.2f ms/image (%d samples)",
                         size == 0 ? "full" : "\(size)", Self.ms(total) / Double(files.count), files.count))
        }
    }

    /// Same-run before/after of the decode alone (JPEGs extracted once up front):
    /// legacy = ImageIO decode + CPU orientation, and grid thumbnails from the
    /// 2048 canonical decode + CGContext downscale; new = `GPUImaging.decode`
    /// straight to size (hardware DCT scaling, cached immediately, GPU orientation).
    @Test func previewDecodeLegacyVsGPU() async throws {
        let files = sampleARWs()
        guard !files.isEmpty else { return }
        var jpegs: [EncodedPreview] = []
        for f in files { jpegs.append(try await RustPreviewDecoder().encodedPreview(for: f)) }
        func legacy(_ e: EncodedPreview, _ size: Int) -> CGImage? {
            guard let src = CGImageSourceCreateWithData(e.jpeg as CFData, nil) else { return nil }
            let img = size > 0
                ? CGImageSourceCreateThumbnailAtIndex(src, 0, [kCGImageSourceCreateThumbnailFromImageAlways: true,
                                                               kCGImageSourceCreateThumbnailWithTransform: false,
                                                               kCGImageSourceThumbnailMaxPixelSize: size] as CFDictionary)
                : CGImageSourceCreateImageAtIndex(src, 0, nil)
            return img.flatMap { ImageOrientation.apply(exif: e.orientation, to: $0) }
        }
        func time(_ body: (EncodedPreview) -> CGImage?) -> Double {
            _ = body(jpegs[0])
            let clock = ContinuousClock(), t = clock.now
            for e in jpegs { #expect(body(e).map(GpuFixtures.pixels)?.isEmpty == false) }
            return Self.ms(clock.now - t) / Double(jpegs.count)
        }
        for size in [256, 1024, 0] {
            let old = time { legacy($0, size) }
            let new = time { GPUImaging.shared.decode(jpeg: $0.jpeg, maxPixelSize: size, exifOrientation: $0.orientation) }
            print(String(format: "GPUMEASURE decode+readback @%@: legacy %.2f ms, gpu %.2f ms per image",
                         size == 0 ? "full" : "\(size)", old, new))
        }
        let oldThumb = time { e in
            legacy(e, 2048).flatMap { c in
                let s = 256.0 / Double(max(c.width, c.height))
                return GPUImaging.cpuDownscale(c, width: Int(Double(c.width) * s), height: Int(Double(c.height) * s))
            }
        }
        let newThumb = time { GPUImaging.shared.decode(jpeg: $0.jpeg, maxPixelSize: 256, exifOrientation: $0.orientation) }
        print(String(format: "GPUMEASURE grid thumbnail 256: legacy canonical+CGContext %.2f ms, direct hw decode %.2f ms",
                     oldThumb, newThumb))
    }

    /// Histogram of a 2560×1707 8-bit frame (the viewer's per-frame cost).
    @Test func histogramCost() async {
        let w = 2560, h = 1707
        var px = [UInt8](repeating: 0, count: w * h * 4)
        for i in stride(from: 0, to: px.count, by: 4) { px[i] = UInt8(i % 251); px[i + 1] = UInt8(i % 241); px[i + 2] = UInt8(i % 239); px[i + 3] = 255 }
        let f = RenderedFrame(width: w, height: h, rgba: Data(px))
        let clock = ContinuousClock()
        _ = HistogramEngine.shared.histogram(for: f)
        var total = Duration.zero
        for _ in 0..<10 {
            let t = clock.now
            let hist = HistogramEngine.shared.histogram(for: f)
            total += clock.now - t
            #expect(hist.pixelCount == w * h)
        }
        print(String(format: "GPUMEASURE histogram 2560x1707: %.2f ms (%@)", Self.ms(total) / 10,
                     HistogramEngine.shared.usesGPU ? "GPU" : "CPU"))
        let cpu = HistogramEngine(device: nil)
        let t = clock.now
        for _ in 0..<3 { _ = cpu.histogram(for: f) }
        print(String(format: "GPUMEASURE histogram 2560x1707 CPU fallback same run: %.2f ms", Self.ms(clock.now - t) / 3))
    }

    /// Grid scroll through 2,000 thumbnails in an offscreen window: per-step cost of
    /// layout + display (the CPU work of one frame; the GPU composite is asynchronous).
    @Test func gridScrollFrameCost() async throws {
        let m = makeModel(counts: [2000])
        await m.start()
        m.browserMode = .grid
        let host = NSHostingView(rootView: BrowserGrid(model: m).frame(width: 1400, height: 900))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1400, height: 900), styleMask: [.borderless],
                              backing: .buffered, defer: false)
        window.contentView = host
        window.orderBack(nil)
        defer { window.orderOut(nil) }
        host.layoutSubtreeIfNeeded()
        try await Task.sleep(for: .milliseconds(300))
        guard let scroll = Self.findScrollView(host) else {
            Issue.record("no scroll view")
            return
        }
        let clock = ContinuousClock()
        var frames: [Double] = []
        for i in 0..<240 {
            let t = clock.now
            scroll.contentView.scroll(to: NSPoint(x: 0, y: CGFloat(i) * 60))
            scroll.reflectScrolledClipView(scroll.contentView)
            host.layoutSubtreeIfNeeded()
            host.displayIfNeeded()
            frames.append(Self.ms(clock.now - t))
            // Let thumbnail loads land, as a real run loop would between frames.
            try await Task.sleep(for: .milliseconds(4))
        }
        let s = frames.sorted()
        print(String(format: "GPUMEASURE grid scroll 2000 thumbs: median %.2f ms, p95 %.2f ms, max %.2f ms per frame (%d frames)",
                     s[s.count / 2], s[s.count * 95 / 100], s.last!, s.count))
        #expect(s[s.count / 2] < 50)
    }

    static func findScrollView(_ v: NSView) -> NSScrollView? {
        if let s = v as? NSScrollView { return s }
        for sub in v.subviews { if let s = findScrollView(sub) { return s } }
        return nil
    }
}
