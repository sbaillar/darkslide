import CoreGraphics
import DarkslideAI
import Foundation
import Testing
@testable import DarkslideKit

/// Pipeline step 3 through the app: each new tool, set the way the app's panels set it
/// (EditState → AdjustmentsCodec → ds-ffi → ds-pipeline), renders a sample RAW and
/// moves the pixels it should, where it should. Lens corrections are off so mask
/// coordinates map straight onto the rendered frame. Skips without samples.
@Suite(.serialized) struct St3RenderTests {
    /// A tightly packed RGBA8 render.
    struct Img {
        let w: Int, h: Int
        let px: [UInt8]

        func rgb(_ x: Int, _ y: Int) -> (Double, Double, Double) {
            let i = (y * w + x) * 4
            return (Double(px[i]), Double(px[i + 1]), Double(px[i + 2]))
        }

        func luma(_ x: Int, _ y: Int) -> Double {
            let (r, g, b) = rgb(x, y)
            return 0.2126 * r + 0.7152 * g + 0.0722 * b
        }

        func chroma(_ x: Int, _ y: Int) -> Double {
            let (r, g, b) = rgb(x, y)
            return max(r, g, b) - min(r, g, b)
        }

        /// Pixels of the normalized rect `r` (x, y down).
        func points(_ r: CGRect) -> [(Int, Int)] {
            let x0 = Int(r.minX * Double(w)), x1 = Int(r.maxX * Double(w))
            let y0 = Int(r.minY * Double(h)), y1 = Int(r.maxY * Double(h))
            return (max(0, y0)..<min(h, y1)).flatMap { y in (max(0, x0)..<min(w, x1)).map { (x: $0, y: y) } }
        }

        func mean(_ r: CGRect, _ f: (Int, Int) -> Double) -> Double {
            let p = points(r)
            return p.reduce(0) { $0 + f($1.0, $1.1) } / Double(max(1, p.count))
        }

        /// Mean absolute Laplacian of luma (fine detail energy) over the frame.
        var detail: Double {
            var s = 0.0
            for y in 1..<(h - 1) {
                for x in 1..<(w - 1) {
                    s += abs(4 * luma(x, y) - luma(x - 1, y) - luma(x + 1, y) - luma(x, y - 1) - luma(x, y + 1))
                }
            }
            return s / Double((w - 2) * (h - 2))
        }

        var lumaStd: Double {
            let all = CGRect(x: 0, y: 0, width: 1, height: 1)
            let m = mean(all, luma)
            return sqrt(mean(all) { pow(luma($0, $1) - m, 2) })
        }
    }

    static let whole = CGRect(x: 0, y: 0, width: 1, height: 1)

    static func photo() -> Photo? {
        guard let url = sampleARWs().first else { return nil }
        return Photo(id: PhotoID(9301), filename: url.lastPathComponent, meta: samplePhoto().meta, url: url)
    }

    static var base: EditState {
        var e = EditState()
        e.lens.enabled = false
        return e
    }

    static func render(_ photo: Photo, _ edit: EditState, masks: [AITarget: VisionMask] = [:]) async throws -> Img {
        let dev = DevelopRenderer()
        if !masks.isEmpty { dev.maskProvider = { _, _ in masks } }
        let f = try await dev.render(photo, edit: edit, request: RenderRequest(maxPixelSize: 480, quality: .preview))
        var px = [UInt8](repeating: 0, count: f.width * f.height * 4)
        f.withBytes { raw in
            for y in 0..<f.height {
                for i in 0..<(f.width * 4) { px[y * f.width * 4 + i] = raw[y * f.bytesPerRow + i] }
            }
        }
        return Img(w: f.width, h: f.height, px: px)
    }

    static func edited(_ change: (inout EditState) -> Void) -> EditState {
        var e = base
        change(&e)
        return e
    }

    static func layer(_ mask: MaskEdit, ev: Double = 1, _ tools: (inout LayerToolsEdit) -> Void = { _ in }) -> LayerEdit {
        var l = LayerEdit(id: 1, name: "L", exposureEV: ev, mask: mask)
        tools(&l.tools)
        return l
    }

    /// Mean luma change inside `r`.
    static func delta(_ a: Img, _ b: Img, _ r: CGRect) -> Double { b.mean(r, b.luma) - a.mean(r, a.luma) }

    // MARK: presence

    @Test func vibranceRaisesMutedColoursMostAndNegativeMutesThem() async throws {
        guard let p = Self.photo() else { return }
        let a = try await Self.render(p, Self.base)
        let up = try await Self.render(p, Self.edited { $0.presence.vibrance = 80 })
        let down = try await Self.render(p, Self.edited { $0.presence.vibrance = -80 })
        // Split by the original chroma: muted vs saturated pixels.
        let chromas = a.points(Self.whole).map { a.chroma($0.0, $0.1) }.sorted()
        let median = chromas[chromas.count / 2]
        var (mutedGain, mutedN, satGain, satN) = (0.0, 0, 0.0, 0)
        for (x, y) in a.points(Self.whole) where a.chroma(x, y) > 4 {
            let g = up.chroma(x, y) / a.chroma(x, y)
            if a.chroma(x, y) < median { mutedGain += g; mutedN += 1 } else { satGain += g; satN += 1 }
        }
        let (muted, saturated) = (mutedGain / Double(max(1, mutedN)), satGain / Double(max(1, satN)))
        let (c0, cUp, cDown) = (a.mean(Self.whole, a.chroma), up.mean(Self.whole, up.chroma),
                                down.mean(Self.whole, down.chroma))
        print(String(format: "ST3 vibrance: chroma %.1f -> +80 %.1f / -80 %.1f; gain muted %.2fx, saturated %.2fx",
                     c0, cUp, cDown, muted, saturated))
        #expect(cUp > c0 * 1.05 && cDown < c0 * 0.95)
        #expect(muted > saturated, "muted colours gain most")
    }

    @Test func textureAddsAndRemovesFineDetail() async throws {
        guard let p = Self.photo() else { return }
        let a = try await Self.render(p, Self.base)
        let up = try await Self.render(p, Self.edited { $0.presence.texture = 80 })
        let down = try await Self.render(p, Self.edited { $0.presence.texture = -80 })
        print(String(format: "ST3 texture: detail %.2f -> +80 %.2f (%.2fx) / -80 %.2f (%.2fx)",
                     a.detail, up.detail, up.detail / a.detail, down.detail, down.detail / a.detail))
        // Clearly visible since the pipeline's texture rescale (PIPELINE_VERSION 5).
        #expect(up.detail > a.detail * 1.2)
        #expect(down.detail < a.detail * 0.9)
        #expect(abs(Self.delta(a, up, Self.whole)) < 3, "texture leaves the tone alone")
    }

    @Test func dehazeAddsContrastAndNegativeAddsHaze() async throws {
        guard let p = Self.photo() else { return }
        let a = try await Self.render(p, Self.base)
        let up = try await Self.render(p, Self.edited { $0.presence.dehaze = 60 })
        let down = try await Self.render(p, Self.edited { $0.presence.dehaze = -60 })
        print(String(format: "ST3 dehaze: luma std %.1f -> +60 %.1f / -60 %.1f", a.lumaStd, up.lumaStd, down.lumaStd))
        #expect(up.lumaStd > a.lumaStd * 1.02)
        #expect(down.lumaStd < a.lumaStd * 0.98)
    }

    // MARK: layers and masks

    @Test func everyMaskShapeConfinesItsLayer() async throws {
        guard let p = Self.photo() else { return }
        let a = try await Self.render(p, Self.base)
        let top = CGRect(x: 0, y: 0, width: 1, height: 0.12), bottom = CGRect(x: 0, y: 0.8, width: 1, height: 0.2)
        // Linear gradient: fully on at its start (the top), off from its end (0.3) on.
        let lin = try await Self.render(p, Self.edited {
            $0.layers = [Self.layer(MaskEdit(shapes: [.linear(.add, start: CGPoint(x: 0.5, y: 0), end: CGPoint(x: 0.5, y: 0.3))]))]
        })
        print(String(format: "ST3 linear: top %.1f (base %.1f), bottom %.1f", Self.delta(a, lin, top), a.mean(top, a.luma),
                     Self.delta(a, lin, bottom)))
        #expect(Self.delta(a, lin, top) > 15 && abs(Self.delta(a, lin, bottom)) < 2,
                "linear: top \(Self.delta(a, lin, top)), bottom \(Self.delta(a, lin, bottom))")
        // Radial: the centre, not the corners.
        let centre = CGRect(x: 0.45, y: 0.45, width: 0.1, height: 0.1), corner = CGRect(x: 0, y: 0, width: 0.1, height: 0.1)
        let rad = try await Self.render(p, Self.edited {
            $0.layers = [Self.layer(MaskEdit(shapes: [.radial(.add, center: CGPoint(x: 0.5, y: 0.5),
                                                              radius: CGSize(width: 0.15, height: 0.15), angle: 0, feather: 0.1)]))]
        })
        #expect(Self.delta(a, rad, centre) > 15 && abs(Self.delta(a, rad, corner)) < 2,
                "radial: centre \(Self.delta(a, rad, centre)), corner \(Self.delta(a, rad, corner))")
        // Brush: along the stroke only.
        let stroke = BrushStroke(points: [CGPoint(x: 0.1, y: 0.5), CGPoint(x: 0.3, y: 0.5)], size: 0.06, hardness: 1)
        let on = CGRect(x: 0.15, y: 0.49, width: 0.1, height: 0.02), off = CGRect(x: 0.7, y: 0.4, width: 0.2, height: 0.2)
        let brush = try await Self.render(p, Self.edited { $0.layers = [Self.layer(MaskEdit(shapes: [.brush(.add, [stroke])]))] })
        #expect(Self.delta(a, brush, on) > 15 && abs(Self.delta(a, brush, off)) < 2,
                "brush: on \(Self.delta(a, brush, on)), off \(Self.delta(a, brush, off))")
        // Sky (the app's AI mask: the top 40% here), darkened.
        let sky = VisionMask(width: 64, height: 64, bytes: (0..<(64 * 64)).map { $0 / 64 < 26 ? 255 : 0 })
        let ai = try await Self.render(p, Self.edited { $0.layers = [Self.layer(MaskEdit(shapes: [.ai(.add, .sky)]), ev: -1)] },
                                       masks: [.sky: sky])
        #expect(Self.delta(a, ai, top) < -10 && abs(Self.delta(a, ai, bottom)) < 2,
                "sky: top \(Self.delta(a, ai, top)), bottom \(Self.delta(a, ai, bottom))")
        // Everything at once, plus a layer carrying presence: each region still moves.
        let all = try await Self.render(p, Self.edited {
            var vib = Self.layer(MaskEdit(shapes: [.fill(.add)]), ev: 0) { $0.presence.vibrance = -100 }
            vib.id = 5
            $0.layers = [
                Self.layer(MaskEdit(shapes: [.linear(.add, start: CGPoint(x: 0.5, y: 0), end: CGPoint(x: 0.5, y: 0.3))])),
                Self.layer(MaskEdit(shapes: [.radial(.add, center: CGPoint(x: 0.5, y: 0.5),
                                                     radius: CGSize(width: 0.15, height: 0.15), angle: 0, feather: 0.1)])),
                Self.layer(MaskEdit(shapes: [.brush(.add, [stroke])])),
                Self.layer(MaskEdit(shapes: [.ai(.add, .sky)]), ev: -1),
                vib,
            ].enumerated().map { i, l in var l = l; l.id = i + 1; return l }
        }, masks: [.sky: sky])
        #expect(Self.delta(a, all, centre) > 10 && Self.delta(a, all, on) > 10)
        #expect(all.mean(Self.whole, all.chroma) < a.mean(Self.whole, a.chroma) * 0.9, "the presence layer desaturates")
    }

    @Test func colourRangeMaskSelectsItsHue() async throws {
        guard let p = Self.photo() else { return }
        let a = try await Self.render(p, Self.base)
        // The photo's most common hue among coloured pixels (30° bins).
        func hue(_ x: Int, _ y: Int) -> Double? {
            let (r, g, b) = a.rgb(x, y)
            let mx = max(r, g, b), mn = min(r, g, b), c = mx - mn
            guard c > 20 else { return nil }
            var h = mx == r ? (g - b) / c : mx == g ? 2 + (b - r) / c : 4 + (r - g) / c
            h *= 60
            return h < 0 ? h + 360 : h
        }
        var bins = [Int](repeating: 0, count: 12)
        for (x, y) in a.points(Self.whole) { if let h = hue(x, y) { bins[Int(h / 30) % 12] += 1 } }
        let centre = Double(bins.indices.max { bins[$0] < bins[$1] }!) * 30 + 15
        let range = ColorRangeEdit(centerHue: centre, width: 40, smoothness: 0.3)
        let b = try await Self.render(p, Self.edited {
            $0.layers = [Self.layer(MaskEdit(shapes: [.fill(.add)], colorRange: range), ev: 0) { $0.saturation = -100 }]
        })
        var (inside, insideN, outside, outsideN) = (0.0, 0, 0.0, 0)
        for (x, y) in a.points(Self.whole) {
            guard let h = hue(x, y) else { continue }
            let d = abs((h - centre + 540).truncatingRemainder(dividingBy: 360) - 180)
            let drop = a.chroma(x, y) - b.chroma(x, y)
            if d < 15 { inside += drop; insideN += 1 } else if d > 90 { outside += drop; outsideN += 1 }
        }
        let (i, o) = (inside / Double(max(1, insideN)), outside / Double(max(1, outsideN)))
        print(String(format: "ST3 colour range %.0f°: chroma drop inside %.1f (%d px), far hues %.1f (%d px)",
                     centre, i, insideN, o, outsideN))
        #expect(insideN > 50 && i > 10, "the picked hue desaturates")
        #expect(outsideN == 0 || o < i * 0.25, "far hues barely move")
    }

    @Test func featherSoftensAMaskEdge() async throws {
        guard let p = Self.photo() else { return }
        let a = try await Self.render(p, Self.base)
        let stroke = BrushStroke(points: [CGPoint(x: 0.2, y: 0.5), CGPoint(x: 0.8, y: 0.5)], size: 0.1, hardness: 1)
        func softPixels(_ feather: Double) async throws -> Int {
            let b = try await Self.render(p, Self.edited {
                $0.layers = [Self.layer(MaskEdit(shapes: [.brush(.add, [stroke])], feather: feather), ev: 2)]
            })
            // Along a column through the stroke: rows partly lifted (between 15% and 85% of the full lift).
            let x = b.w / 2
            let lifts = (0..<b.h).map { b.luma(x, $0) - a.luma(x, $0) }
            let full = lifts.max() ?? 0
            return lifts.filter { $0 > full * 0.15 && $0 < full * 0.85 }.count
        }
        let hard = try await softPixels(0), soft = try await softPixels(0.03)
        print("ST3 feather: partly-lifted rows \(hard) (feather 0) -> \(soft) (feather 0.03)")
        #expect(soft > hard + 4)
    }

    @Test func autoMaskKeepsABrushInsideEdges() async throws {
        guard let p = Self.photo() else { return }
        let a = try await Self.render(p, Self.base)
        let points = (0...8).map { CGPoint(x: 0.1 + Double($0) * 0.1, y: 0.5) }
        func lifted(_ auto: Bool) async throws -> Set<Int> {
            let s = BrushStroke(points: points, size: 0.15, hardness: 1, autoMask: auto)
            let b = try await Self.render(p, Self.edited { $0.layers = [Self.layer(MaskEdit(shapes: [.brush(.add, [s])]), ev: 2)] })
            return Set(a.points(Self.whole).filter { b.luma($0.0, $0.1) - a.luma($0.0, $0.1) > 8 }.map { $0.1 * a.w + $0.0 })
        }
        let plain = try await lifted(false), auto = try await lifted(true)
        let inside = Double(auto.intersection(plain).count) / Double(max(1, auto.count))
        print("ST3 auto-mask: \(plain.count) px lifted plain, \(auto.count) auto-masked (\(Int(inside * 100))% inside the stroke)")
        #expect(!auto.isEmpty && auto.count < plain.count, "edges stop the auto-masked stroke")
        #expect(inside > 0.95)
    }

    // MARK: spots

    @Test func cloneCopiesItsSourceAndHealKeepsTheSurroundingTone() async throws {
        guard let p = Self.photo() else { return }
        let a = try await Self.render(p, Self.base)
        let dest = CGPoint(x: 0.3, y: 0.5), src = CGPoint(x: 0.7, y: 0.3)
        let r = 0.03
        // The spot's core (in pixels of the long edge), and a patch far from both.
        func core(_ c: CGPoint) -> CGRect {
            let rx = r * Double(max(a.w, a.h)) / Double(a.w) * 0.5, ry = r * Double(max(a.w, a.h)) / Double(a.h) * 0.5
            return CGRect(x: c.x - rx, y: c.y - ry, width: 2 * rx, height: 2 * ry)
        }
        let far = CGRect(x: 0.05, y: 0.85, width: 0.1, height: 0.1)
        let clone = try await Self.render(p, Self.edited {
            $0.spots = [SpotEdit(kind: .clone, center: dest, radius: r, source: src, feather: 0)]
        })
        let (before, source, after) = (a.mean(core(dest), a.luma), a.mean(core(src), a.luma), clone.mean(core(dest), clone.luma))
        print(String(format: "ST3 clone: dest %.1f -> %.1f (source %.1f)", before, after, source))
        #expect(abs(after - source) < max(3, abs(before - source) * 0.2), "the destination shows the source")
        #expect(abs(Self.delta(a, clone, far)) < 1)
        let heal = try await Self.render(p, Self.edited {
            $0.spots = [SpotEdit(kind: .heal, center: dest, radius: r, source: src, feather: 0.5)]
        })
        let changed = a.points(core(dest)).contains { abs(heal.luma($0.0, $0.1) - a.luma($0.0, $0.1)) > 2 }
        let ring = CGRect(x: dest.x - 0.08, y: dest.y - 0.08, width: 0.16, height: 0.16)
        print(String(format: "ST3 heal: dest %.1f -> %.1f, surroundings %.1f", before, heal.mean(core(dest), heal.luma),
                     a.mean(ring, a.luma)))
        #expect(changed || abs(before - source) < 2, "heal replaces the destination's texture")
        #expect(abs(Self.delta(a, heal, far)) < 1)
    }
}
