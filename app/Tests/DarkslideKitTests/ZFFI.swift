import Foundation
import Metal
import CoreGraphics
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI
import DarkslideFFI

@Suite(.serialized) struct ZFFI {
    static func ms(_ d: Duration) -> Double { Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15 }
    static func photo(_ w: Int, _ h: Int) -> Photo {
        var p = MockSessionStore.makePhotos(count: 1, firstID: 1, start: Date())[0]
        p.meta.pixelWidth = w; p.meta.pixelHeight = h
        return p
    }

    @Test func cancelLatency1to1() async throws {
        let r = RustRenderer()
        let p = Self.photo(6000, 4000)
        for fmt in [FramePixelFormat.rgba8Srgb, .rgba16fLinearP3] {
            let req = RenderRequest(maxPixelSize: 6000, scale: 1.0, format: fmt)
            let c = ContinuousClock()
            var t = c.now
            let f = try await r.render(p, edit: EditState(), request: req)
            let full = Self.ms(c.now - t)
            var lat: [Double] = []
            for delay in [1, 5, 20, 50] {
                let task = Task.detached { try await r.render(p, edit: EditState(), request: req) }
                try await Task.sleep(for: .milliseconds(delay))
                t = c.now
                task.cancel()
                let res = await task.result
                lat.append(Self.ms(c.now - t))
                if case .success = res { print("ZFFI note: delay \(delay) completed before cancel") }
            }
            print("ZFFI 1:1 6000x4000 \(fmt) \(f.width)x\(f.height) full=\(String(format: "%.1f", full)) ms, cancel latency ms=\(lat.map { String(format: "%.2f", $0) })")
        }
    }

    @Test func cancelUnderSaturatedPool() async throws {
        let r = RustRenderer()
        let p = Self.photo(6000, 4000)
        let req = RenderRequest(maxPixelSize: 6000, scale: 1.0, format: .rgba16fLinearP3)
        let n = ProcessInfo.processInfo.activeProcessorCount * 2
        let c = ContinuousClock()
        let busy = (0..<n).map { _ in Task.detached { try await r.render(p, edit: EditState(), request: req) } }
        try await Task.sleep(for: .milliseconds(10))
        let queued = Task.detached { try await r.render(p, edit: EditState(), request: req) }
        try await Task.sleep(for: .milliseconds(10))
        var t = c.now
        queued.cancel()
        let qr = await queued.result
        let qLat = Self.ms(c.now - t)
        t = c.now
        busy.forEach { $0.cancel() }
        for b in busy { _ = await b.result }
        print("ZFFI saturated(\(n) busy): queued cancel latency=\(String(format: "%.2f", qLat)) ms result=\(qr); cancel all \(n) running=\(String(format: "%.1f", Self.ms(c.now - t))) ms")
    }

    /// Does Rust touch caller memory after the call returned Cancelled?
    @Test func noWritesAfterCancelledReturn() async throws {
        let e = try Engine()
        let req = DarkslideFFI.RenderRequest(maxLongEdge: 6000, scale: 1, region: nil, format: .rgba8Srgb)
        let info = try planFrame(sourceWidth: 6000, sourceHeight: 4000, request: req)
        let len = Int(info.bytesPerRow) * Int(info.height)
        var violations = 0
        for i in 0..<40 {
            let buf = UnsafeMutableRawPointer.allocate(byteCount: len, alignment: 256)
            memset(buf, 0, len)
            let token = try CancelToken(); let addr = UInt64(UInt(bitPattern: buf))
            let task = Task.detached {
                try await withTaskCancellationHandler {
                    try await e.renderTestPatternInto(sourceWidth: 6000, sourceHeight: 4000,
                        params: TestPatternParams(exposureEv: 0, warmth: 0, saturation: 0), request: req,
                        destPtr: addr, destLen: UInt64(len), token: token)
                } onCancel: { try? token.cancel() }
            }
            try await Task.sleep(for: .microseconds(200 * (i % 10)))
            task.cancel()
            _ = await task.result
            let snap = Data(bytes: buf, count: len)
            try await Task.sleep(for: .milliseconds(20))
            if Data(bytes: buf, count: len) != snap { violations += 1 }
            buf.deallocate()
        }
        print("ZFFI writes-after-return violations=\(violations)/40")
        #expect(violations == 0)
    }

    @Test func raceStress() async throws {
        let r = RustRenderer()
        let p = Self.photo(3000, 2000)
        var done = 0, cancelled = 0, other = 0
        for i in 0..<400 {
            let task = Task.detached { try await r.render(p, edit: EditState(), request: RenderRequest(maxPixelSize: 3000, scale: 1)) }
            try await Task.sleep(for: .microseconds((i * 7919) % 40_000))
            task.cancel()
            switch await task.result {
            case .success: done += 1
            case .failure(let e) where e is CancellationError: cancelled += 1
            case .failure(let e): other += 1; print("ZFFI other error \(e)")
            }
        }
        print("ZFFI race: done=\(done) cancelled=\(cancelled) other=\(other)")
        #expect(other == 0)
    }

    @Test func previewAndMetadataOnSamples() async throws {
        let raws = sampleARWs()
        print("ZFFI samples: \(raws.count) raws")
        #expect(raws.allSatisfy { RustRaw.isSupported($0) })
        let prov = RustPreviewProvider()
        let c = ContinuousClock()
        for u in raws.prefix(6) {
            let t = c.now
            let ph = try await RustRaw.photo(at: u, id: PhotoID(1))
            let t2 = c.now
            let img: CGImage? = try await prov.preview(for: u, maxPixelSize: 0)
            print("ZFFI \(u.lastPathComponent) meta=\(String(format: "%.2f", Self.ms(t2 - t)))ms preview=\(String(format: "%.2f", Self.ms(c.now - t2)))ms meta \(ph.meta.pixelWidth)x\(ph.meta.pixelHeight) preview \(String(describing: img.map { ($0.width, $0.height) }))")
        }
    }
}
