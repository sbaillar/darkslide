import CoreGraphics
import CoreML
import DarkslideAI
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// People sub-part masks: Vision → E-frame geometry, face-crop placement, composition on
/// synthetic people, and the plumbing into the pipeline's mask sources.
struct PeopleGeometryTests {
    @Test func visionCoordinatesBecomeTopLeftPixels() {
        #expect(VisionGeometry.point(CGPoint(x: 0.25, y: 0.75), width: 200, height: 100) == CGPoint(x: 50, y: 25))
        let r = VisionGeometry.rect(CGRect(x: 0.1, y: 0.6, width: 0.2, height: 0.3), width: 100, height: 100)
        #expect(abs(r.minX - 10) < 1e-9 && abs(r.minY - 10) < 1e-9 && abs(r.width - 20) < 1e-9 && abs(r.height - 30) < 1e-9)
        #expect(VisionGeometry.flipped(CGPoint(x: 3, y: 10), height: 40) == CGPoint(x: 3, y: 30))
        #expect(VisionGeometry.centroid([]) == nil)
        #expect(VisionGeometry.centroid([CGPoint(x: 0, y: 0), CGPoint(x: 2, y: 4)]) == CGPoint(x: 1, y: 2))
    }

    @Test func faceAngleFromLandmarksInAnyOrientation() throws {
        let eyesUp = try #require(FaceCrop.angle(leftEye: [CGPoint(x: 10, y: 10)], rightEye: [CGPoint(x: 20, y: 10)],
                                                 mouth: [CGPoint(x: 15, y: 30)]))
        #expect(abs(eyesUp) < 1e-9)
        let upsideDown = try #require(FaceCrop.angle(leftEye: [CGPoint(x: 10, y: 30)], rightEye: [CGPoint(x: 20, y: 30)],
                                                     mouth: [CGPoint(x: 15, y: 10)]))
        #expect(abs(abs(upsideDown) - .pi) < 1e-9)
        let headRight = try #require(FaceCrop.angle(leftEye: [CGPoint(x: 30, y: 10)], rightEye: [CGPoint(x: 30, y: 20)],
                                                    mouth: [CGPoint(x: 10, y: 15)]))
        #expect(abs(headRight - .pi / 2) < 1e-9, "the top of the head points right: a clockwise quarter turn")
        #expect(FaceCrop.angle(leftEye: [], rightEye: [CGPoint.zero], mouth: [CGPoint.zero]) == nil)
        #expect(FaceCrop.angle(leftEye: [CGPoint.zero], rightEye: [CGPoint.zero], mouth: [CGPoint.zero]) == nil)
    }

    @Test func faceCropMapsBothWays() {
        let box = CGRect(x: 100, y: 100, width: 58, height: 58)
        let up = FaceCrop.around(face: box, angle: 0, size: 100)
        #expect(abs(up.side - 100) < 1e-9)
        #expect(abs(Double(up.center.y) - (129 - 8)) < 1e-9, "the crop sits above the face's centre")
        #expect(up.toFrame(CGPoint(x: 50, y: 50)) == up.center)
        #expect(abs(up.toFrame(CGPoint(x: 0, y: 0)).x - 79) < 1e-9)
        for angle in [0.0, 0.7, .pi, -2.0] {
            let c = FaceCrop.around(face: box, angle: angle, size: 64)
            let p = CGPoint(x: 13, y: 51)
            let back = c.toCrop(c.toFrame(p))
            #expect(abs(back.x - p.x) < 1e-6 && abs(back.y - p.y) < 1e-6)
        }
        // Upside down: the crop's top row lies below the face in the frame.
        let down = FaceCrop.around(face: box, angle: .pi, size: 100)
        #expect(down.toFrame(CGPoint(x: 50, y: 0)).y > box.maxY)
        let b = up.frameBounds(width: 150, height: 150)
        #expect(b?.x0 == 79 && b?.x1 == 150 && b?.y0 == 71)
        #expect(FaceCrop.around(face: CGRect(x: 500, y: 500, width: 10, height: 10), angle: 0).frameBounds(width: 50, height: 50) == nil)
    }

    @Test func extractSamplesTheTurnedCrop() {
        // Left half red, right half blue; a crop turned upside down swaps them.
        let w = 40, h = 40
        var px = [UInt8](repeating: 255, count: w * h * 4)
        for i in 0..<(w * h) {
            let left = i % w < 20
            px[i * 4] = left ? 255 : 0
            px[i * 4 + 1] = 0
            px[i * 4 + 2] = left ? 0 : 255
        }
        let img = PixelImage(width: w, height: h, rgba: px)
        let c = FaceCrop(center: CGPoint(x: 20, y: 20), side: 20, angle: .pi, size: 8)
        let crop = c.extract(from: img)
        #expect(crop[0] == 0 && crop[2] == 255, "crop's left is the frame's right")
        #expect(crop[(8 * 8 - 1) * 4] == 255)
        #expect(img.sample(-5, -5) == SIMD3(255, 0, 0))
    }

    /// A render of just the crop's region, at twice the frame's resolution, gives the same
    /// crop as the whole frame (sampled through the region's mapping).
    @Test func extractFromARegionRender() {
        let w = 40, h = 40
        func gradient(_ w: Int, _ h: Int, _ f: (Int, Int) -> (UInt8, UInt8)) -> PixelImage {
            var px = [UInt8](repeating: 255, count: w * h * 4)
            for y in 0..<h { for x in 0..<w { (px[(y * w + x) * 4], px[(y * w + x) * 4 + 1]) = f(x, y) } }
            return PixelImage(width: w, height: h, rgba: px)
        }
        let frame = gradient(w, h) { x, y in (UInt8(x * 6), UInt8(y * 6)) }
        // The region x 10..30, y 5..25 at 2×: pixel (i, j) shows frame (10 + i/2, 5 + j/2).
        let region = CGRect(x: 0.25, y: 0.125, width: 0.5, height: 0.5)
        let detail = gradient(40, 40) { i, j in (UInt8((10 + i / 2) * 6), UInt8((5 + j / 2) * 6)) }
        let c = FaceCrop(center: CGPoint(x: 20, y: 15), side: 12, angle: 0.3, size: 8)
        let a = c.extract(from: frame), b = c.extract(from: detail, region: region, frameWidth: w, frameHeight: h)
        let diff = zip(a, b).map { abs(Int($0) - Int($1)) }.max() ?? 0
        #expect(diff <= 4, "same crop from the region render (max diff \(diff))")
    }

    @Test func pixelImageFromCGImage() throws {
        let ctx = CGContext(data: nil, width: 3, height: 2, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        ctx.setFillColor(CGColor(srgbRed: 0, green: 1, blue: 0, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 1, width: 3, height: 1)) // CG's top row
        let img = try #require(PixelImage(ctx.makeImage()!))
        #expect(img.width == 3 && img.rgba[1] == 255 && img.rgba[3 * 4 + 1] == 0, "row 0 is the top")
    }

    @Test func polygonsRasterSoftly() {
        let sq = [CGPoint(x: 1, y: 1), CGPoint(x: 3, y: 1), CGPoint(x: 3, y: 3), CGPoint(x: 1, y: 3)]
        #expect(PolygonRaster.contains(sq, CGPoint(x: 2, y: 2)) && !PolygonRaster.contains(sq, CGPoint(x: 0, y: 2)))
        #expect(!PolygonRaster.contains([CGPoint.zero], .zero))
        var m = [UInt8](repeating: 0, count: 16)
        PolygonRaster.fill(sq, into: &m, width: 4, height: 4)
        #expect(m[5] == 255 && m[0] == 0)
        let half = [CGPoint(x: 0.5, y: 0), CGPoint(x: 1, y: 0), CGPoint(x: 1, y: 1), CGPoint(x: 0.5, y: 1)]
        var n = [UInt8](repeating: 0, count: 4)
        PolygonRaster.fill(half, into: &n, width: 2, height: 2)
        #expect(n[0] == 127)
        PolygonRaster.fill([CGPoint(x: 9, y: 9), CGPoint(x: 10, y: 9), CGPoint(x: 10, y: 10)], into: &n, width: 2, height: 2)
        PolygonRaster.fill([], into: &n, width: 2, height: 2)
    }
}

struct PeopleComposerTests {
    static let skin: [UInt8] = [224, 172, 140]
    static let cloth: [UInt8] = [30, 60, 200]
    static let hair: [UInt8] = [60, 40, 25]

    /// A 40×40 "person": hair rows 0-7, skin rows 8-19, clothes rows 20-39 in columns
    /// 10-29; grey background elsewhere.
    static func person() -> (PixelImage, [Float]) {
        let w = 40, h = 40
        var px = [UInt8](repeating: 255, count: w * h * 4)
        var matte = [Float](repeating: 0, count: w * h)
        for y in 0..<h {
            for x in 0..<w {
                let i = y * w + x
                var c: [UInt8] = [128, 128, 128]
                if x >= 10, x < 30 {
                    matte[i] = 1
                    c = y < 8 ? hair : y < 20 ? skin : cloth
                }
                px[i * 4] = c[0]; px[i * 4 + 1] = c[1]; px[i * 4 + 2] = c[2]
            }
        }
        return (PixelImage(width: w, height: h, rgba: px), matte)
    }

    static func mean(_ m: [UInt8]?, rows: Range<Int>, cols: Range<Int> = 10..<30, width w: Int = 40) -> Double {
        guard let m else { return -1 }
        var s = 0.0, n = 0.0
        for y in rows { for x in cols { s += Double(m[y * w + x]); n += 1 } }
        return s / n / 255
    }

    @Test func noFaceGenericSkinAndClothes() {
        let (img, matte) = Self.person()
        let parts = PeopleComposer.compose(image: img, observation: PeopleObservation(width: 40, height: 40, mattes: [matte]),
                                           parses: [])
        #expect(parts.count == 1)
        let m = parts[0].masks
        #expect(Set(m.keys) == Set(PeopleTargets.parts))
        #expect(Self.mean(m[.skin], rows: 10..<18) > 0.9 && Self.mean(m[.skin], rows: 24..<38) < 0.05)
        #expect(Self.mean(m[.clothes], rows: 24..<38) > 0.9 && Self.mean(m[.clothes], rows: 10..<18) < 0.1)
        #expect(Self.mean(m[.person], rows: 0..<40) == 1 && Self.mean(m[.person], rows: 0..<40, cols: 0..<10) == 0)
        #expect(Self.mean(m[.face], rows: 0..<40) == 0 && Self.mean(m[.hair], rows: 0..<40) == 0)
        #expect(PeopleComposer.compose(image: img, observation: PeopleObservation(width: 4, height: 4, mattes: []),
                                       parses: []).isEmpty)
    }

    @Test func aHeadSeenFromBehindIsHair() {
        let (img, matte) = Self.person()
        let head = HeadHint(center: CGPoint(x: 20, y: 6), radius: 7, facing: false, up: CGPoint(x: 0, y: -1))
        let o = PeopleObservation(width: 40, height: 40, mattes: [matte], heads: [head])
        let m = PeopleComposer.compose(image: img, observation: o, parses: [])[0].masks
        #expect(Self.mean(m[.hair], rows: 1..<6, cols: 16..<24) > 0.8)
        #expect(Self.mean(m[.hair], rows: 30..<38) < 0.05)
        #expect(Self.mean(m[.clothes], rows: 1..<6, cols: 16..<24) < 0.2)
    }

    @Test func aFacingHeadWithoutAFaceGetsAFace() {
        let (img, matte) = Self.person()
        let head = HeadHint(center: CGPoint(x: 20, y: 14), radius: 5, facing: true)
        let m = PeopleComposer.compose(image: img, observation: PeopleObservation(width: 40, height: 40, mattes: [matte],
                                                                                 heads: [head]), parses: [])[0].masks
        #expect(Self.mean(m[.face], rows: 12..<16, cols: 18..<22) > 0.8)
    }

    /// A face parse: an 8×8 crop over rows 0-19 whose top quarter is hair, then skin with
    /// eyes, a mouth and lips; the frame also has white teeth inside the inner lips.
    @Test func faceParseIsMappedBackAndCalibratesSkin() {
        var (img, matte) = Self.person()
        var px = img.rgba
        for y in 15..<17 { for x in 18..<22 { let i = (y * 40 + x) * 4; px[i] = 245; px[i + 1] = 245; px[i + 2] = 240 } }
        img = PixelImage(width: 40, height: 40, rgba: px)
        let crop = FaceCrop(center: CGPoint(x: 20, y: 10), side: 20, angle: 0, size: 8)
        var labels = [UInt8](repeating: FaceParsingClass.skin.rawValue, count: 64)
        for i in 0..<16 { labels[i] = FaceParsingClass.hair.rawValue }
        labels[3 * 8 + 3] = FaceParsingClass.leftEye.rawValue
        labels[6 * 8 + 4] = FaceParsingClass.mouth.rawValue
        labels[7 * 8 + 0] = FaceParsingClass.cloth.rawValue
        let face = FaceHint(box: CGRect(x: 14, y: 6, width: 12, height: 12), angle: 0,
                            leftEye: [CGPoint(x: 22, y: 8), CGPoint(x: 27, y: 8), CGPoint(x: 27, y: 12), CGPoint(x: 22, y: 12)],
                            rightEye: [], innerLips: [CGPoint(x: 18, y: 15), CGPoint(x: 22, y: 15), CGPoint(x: 22, y: 17),
                                                      CGPoint(x: 18, y: 17)])
        let o = PeopleObservation(width: 40, height: 40, mattes: [matte], faces: [face])
        let m = PeopleComposer.compose(image: img, observation: o, parses: [FaceParse(crop: crop, labels: labels)])[0].masks
        #expect(Self.mean(m[.hair], rows: 1..<4, cols: 12..<28) > 0.8)
        #expect(Self.mean(m[.face], rows: 8..<14, cols: 12..<28) > 0.8)
        #expect(Self.mean(m[.skin], rows: 8..<14, cols: 12..<18) > 0.8)
        #expect(Self.mean(m[.eyes], rows: 9..<11, cols: 23..<26) > 0.9, "the landmark eye")
        #expect(Self.mean(m[.eyes], rows: 8..<10, cols: 18..<20) > 0.3, "the parsed eye")
        #expect(Self.mean(m[.teeth], rows: 15..<17, cols: 18..<22) > 0.4)
        #expect(Self.mean(m[.teeth], rows: 10..<12, cols: 12..<16) == 0)
        #expect(Self.mean(m[.clothes], rows: 24..<38) > 0.9)
    }

    @Test func projectWeighsLabelsBilinearly() {
        // One crop pixel per frame pixel: quadrants of hair, skin, cloth and mouth.
        let crop = FaceCrop(center: CGPoint(x: 2, y: 2), side: 4, angle: 0, size: 4)
        let labels: [UInt8] = (0..<16).map { i -> FaceParsingClass in
            let (x, y) = (i % 4 < 2, i / 4 < 2)
            return x && y ? .hair : y ? .skin : x ? .cloth : .mouth
        }.map(\.rawValue)
        let m = PeopleComposer.project(FaceParse(crop: crop, labels: labels), width: 4, height: 4)
        #expect(m.hair[0] == 1 && m.skin[3] == 1 && m.clothes[12] == 1 && m.mouth[15] == 1 && m.face[3] == 1)
        #expect(m.covered.allSatisfy { $0 })
        // Coarser crop: soft between classes.
        let coarse = PeopleComposer.project(FaceParse(crop: FaceCrop(center: CGPoint(x: 2, y: 2), side: 4, angle: 0, size: 2),
                                                      labels: [17, 1, 16, 11]), width: 4, height: 4)
        #expect(coarse.hair.contains { $0 > 0 && $0 < 1 })
        let short = PeopleComposer.project(FaceParse(crop: crop, labels: [1]), width: 4, height: 4)
        #expect(short.covered.allSatisfy { !$0 })
    }

    @Test func colorModels() throws {
        let skin = ColorModel.ycc(SIMD3(224, 172, 140))
        #expect(ColorModel.genericSkin.probability(skin) > 0.9)
        #expect(ColorModel.genericSkin.probability(ColorModel.ycc(SIMD3(30, 60, 200))) == 0)
        #expect(ColorModel.fit([skin], chromaOnly: true) == nil)
        let samples = (0..<20).map { SIMD3<Float>(100 + Float($0), 120 + Float($0 % 3), 140) }
        let full = try #require(ColorModel.fit(samples, chromaOnly: false))
        #expect(full.probability(samples[10]) > 0.9 && full.probability(SIMD3(0, 0, 0)) == 0)
        let chroma = try #require(ColorModel.fit(samples, chromaOnly: true))
        #expect(chroma.probability(SIMD3(0, 121, 140)) > 0.9, "luma is free")
        #expect(ColorModel.invert3([1, 2, 3, 2, 4, 6, 0, 0, 1]) == nil)
        #expect(PeopleComposer.smoothstep(1, 1, 0) == 0 && PeopleComposer.smoothstep(1, 1, 2) == 1)
        let e = PeopleComposer.ellipse(CGRect(x: 2, y: 2, width: 6, height: 6), angle: 0.3, width: 10, height: 10)
        #expect(e[55] == 1 && e[0] == 0)
        #expect(PeopleComposer.disk(.zero, 0, width: 4, height: 4).isEmpty)
        #expect(PeopleComposer.disk(CGPoint(x: -50, y: -50), 2, width: 4, height: 4).isEmpty)
    }

    @Test func facesAndHeadsGoToThePersonCoveringThem() {
        var a = [Float](repeating: 0, count: 100), b = a
        for i in 0..<100 { if i % 10 < 5 { a[i] = 1 } else { b[i] = 1 } }
        #expect(PeopleComposer.owner(of: CGRect(x: 6, y: 0, width: 3, height: 3), mattes: [a, b], width: 10, height: 10) == 1)
        #expect(PeopleComposer.owner(of: CGRect(x: 50, y: 50, width: 3, height: 3), mattes: [a, b], width: 10, height: 10) == nil)
    }
}

struct PeopleVisionTests {
    @Test func mattesAreSplitAndOrderedLargestFirst() {
        let w = 20, h = 10
        var small = [Float](repeating: 0, count: w * h), big = small, seg = small
        for y in 0..<h {
            for x in 0..<w {
                if x < 4 { small[y * w + x] = 1 }
                if x >= 8 { big[y * w + x] = 1 }
                if x < 4 || x >= 8 { seg[y * w + x] = 0.9 }
            }
        }
        let out = PeopleMattes.split(instances: [small, big], segmentation: seg, width: w, height: h)
        #expect(out.count == 2 && out[0][9] > 0.8 && out[0][0] == 0 && out[1][0] > 0.8)
        #expect(PeopleMattes.split(instances: [small], segmentation: nil, width: w, height: h) == [small])
        #expect(PeopleMattes.split(instances: [], segmentation: nil, width: w, height: h).isEmpty)
        // A person no instance covers is added from the segmentation.
        let extra = PeopleMattes.split(instances: [big], segmentation: seg, width: w, height: h)
        #expect(extra.count == 2)
        let fromSeg = PeopleMattes.split(instances: [], segmentation: seg, width: w, height: h)
        #expect(fromSeg.count == 2)
        #expect(PeopleMattes.components([Float](repeating: 0.9, count: 4) + [0, 0], width: 3, height: 2).count == 1)
        #expect(PeopleMattes.boxBlur([0, 3, 0], radius: 1, width: 3, height: 1) == [1.5, 1, 1.5])
    }

    @Test func headsFromBodyPose() throws {
        let ears = try #require(HeadHint.from(joints: ["left_ear_joint": CGPoint(x: 10, y: 20), "right_ear_joint": CGPoint(x: 30, y: 20),
                                                       "neck_1_joint": CGPoint(x: 20, y: 40)], imageLong: 1000))
        #expect(abs(ears.radius - 15) < 1e-9 && !ears.facing)
        #expect(ears.up == CGPoint(x: 0, y: -1) && ears.center.y < 20)
        let nose = try #require(HeadHint.from(joints: ["nose_joint": CGPoint(x: 5, y: 5)], imageLong: 1000))
        #expect(nose.facing && nose.radius == 30 && nose.center == CGPoint(x: 5, y: 5))
        let diver = try #require(HeadHint.from(joints: ["neck_1_joint": CGPoint(x: 50, y: 50), "root": CGPoint(x: 50, y: 10),
                                                        "left_shoulder_1_joint": CGPoint(x: 40, y: 50),
                                                        "right_shoulder_1_joint": CGPoint(x: 60, y: 50)], imageLong: 1000))
        #expect(diver.center.y > 50, "upside down: the head is below the neck")
        let upright = try #require(HeadHint.from(joints: ["neck_1_joint": CGPoint(x: 50, y: 50)], imageLong: 100))
        #expect(upright.center.y < 50)
        #expect(HeadHint.from(joints: [:], imageLong: 100) == nil)
    }

    @Test func maskBuffersAreScaled() throws {
        var pb: CVPixelBuffer?
        CVPixelBufferCreate(nil, 2, 1, kCVPixelFormatType_OneComponent8, nil, &pb)
        let b = try #require(pb)
        CVPixelBufferLockBaseAddress(b, [])
        let p = CVPixelBufferGetBaseAddress(b)!.assumingMemoryBound(to: UInt8.self)
        p[0] = 0; p[1] = 255
        CVPixelBufferUnlockBaseAddress(b, [])
        #expect(PeopleVision.scaled(b, width: 4, height: 2) == [0, 0, 1, 1, 0, 0, 1, 1])
        var fb: CVPixelBuffer?
        CVPixelBufferCreate(nil, 1, 1, kCVPixelFormatType_OneComponent32Float, nil, &fb)
        let f = try #require(fb)
        CVPixelBufferLockBaseAddress(f, [])
        CVPixelBufferGetBaseAddress(f)!.assumingMemoryBound(to: Float.self)[0] = 2
        CVPixelBufferUnlockBaseAddress(f, [])
        #expect(PeopleVision.scaled(f, width: 1, height: 1) == [1])
    }

    @Test func visionOnAnImageWithoutPeople() throws {
        let ctx = CGContext(data: nil, width: 64, height: 64, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        let o = try PeopleVision.observe(ctx.makeImage()!)
        #expect(o.width == 64 && o.mattes.isEmpty && o.faces.isEmpty)
    }

    /// A real photo (a person seen from behind, `SKB06368`; skipped without
    /// samples): one person, a head from the body pose, and hair on it.
    @Test func aRealPersonFromBehind() async throws {
        guard let url = sampleARWs().first(where: { $0.lastPathComponent == "SKB06368.ARW" }),
              let img = PeopleLookTests.load(url.path, maxPixelSize: 512) else { return }
        let o = try PeopleVision.observe(img)
        #expect(o.mattes.count >= 1 && !o.heads.isEmpty)
        let sums = o.mattes.map { $0.reduce(0, +) }
        #expect(sums == sums.sorted(by: >), "largest first")
        let masks = try await PeopleMaskService(parser: { nil }).masks(for: img)
        #expect((masks.all[.hair]?.coverage ?? 0) > 0.002 && (masks.all[.skin]?.coverage ?? 0) > 0.02)
    }

    @Test func faceParserLabels() throws {
        let a = try MLMultiArray(shape: [1, 2, 2], dataType: .float32)
        for (i, v) in [0, 17, 30, -1].enumerated() { a[i] = NSNumber(value: v) }
        #expect(try CoreMLFaceParser.labels(a, count: 4) == [0, 17, 18, 0])
        let h = try MLMultiArray(shape: [2], dataType: .float16)
        h[0] = 1; h[1] = 16
        #expect(try CoreMLFaceParser.labels(h, count: 2) == [1, 16])
        let i = try MLMultiArray(shape: [2], dataType: .int32)
        i[0] = 4; i[1] = 5
        #expect(try CoreMLFaceParser.labels(i, count: 2) == [4, 5])
        let d = try MLMultiArray(shape: [1], dataType: .double)
        d[0] = 13
        #expect(try CoreMLFaceParser.labels(d, count: 1) == [13])
        let bad = try MLMultiArray(shape: [3], dataType: .float32)
        bad[0] = NSNumber(value: Float.nan); bad[1] = NSNumber(value: Float.infinity); bad[2] = 2
        #expect(try CoreMLFaceParser.labels(bad, count: 3) == [0, 0, 2], "non-finite ids are background")
        #expect(throws: PeopleMaskError.parser, "too short") { try CoreMLFaceParser.labels(d, count: 3) }
        // Padded rows (strides wider than the shape): read through the strides.
        let padded = UnsafeMutablePointer<Int32>.allocate(capacity: 6)
        padded.initialize(from: [1, 2, 99, 3, 4, 99], count: 6)
        let p = try MLMultiArray(dataPointer: padded, shape: [2, 2], dataType: .int32, strides: [3, 1]) { $0.deallocate() }
        #expect(try CoreMLFaceParser.labels(p, count: 4) == [1, 2, 3, 4])
        #expect(CoreMLFaceParser.denseStrides([1, 2, 3]) == [6, 3, 1] && CoreMLFaceParser.denseStrides([]) == [])
        // An element type it doesn't know is refused, not read as Double.
        let unknown = try MLMultiArray(shape: [2], dataType: .int8)
        #expect(throws: PeopleMaskError.parser) { try CoreMLFaceParser.labels(unknown, count: 2) }

        let stub = StubMLModel()
        stub.output = { input in
            #expect(input.featureValue(for: "input")?.imageBufferValue != nil)
            let out = try MLMultiArray(shape: [1, 512, 512], dataType: .float32)
            out[5] = 17
            return try MLDictionaryFeatureProvider(dictionary: ["argmax_out": MLFeatureValue(multiArray: out)])
        }
        let parser = CoreMLFaceParser(model: stub)
        let labels = try parser.parse(rgba: [UInt8](repeating: 0, count: 512 * 512 * 4))
        #expect(labels.count == 512 * 512 && labels[5] == 17)
        stub.output = { _ in try MLDictionaryFeatureProvider(dictionary: [:]) }
        #expect(throws: PeopleMaskError.parser) { try parser.parse(rgba: [UInt8](repeating: 0, count: 512 * 512 * 4)) }
    }
}

/// The service, the mask sources the pipeline calls, and the panel's actions.
@MainActor
struct PeopleWiringTests {
    struct Parser: FaceParser {
        let inputSize = 8
        func parse(rgba: [UInt8]) throws -> [UInt8] { [UInt8](repeating: FaceParsingClass.skin.rawValue, count: 64) }
    }

    static func image() -> CGImage {
        let ctx = CGContext(data: nil, width: 40, height: 40, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        return ctx.makeImage()!
    }

    final class Counter: @unchecked Sendable {
        let lock = NSLock()
        var n = 0
    }

    static func service(_ counter: Counter, persons: Int = 2, fail: Bool = false) -> PeopleMaskService {
        PeopleMaskService(observe: { img in
            counter.lock.withLock { counter.n += 1 }
            if fail { throw PeopleMaskError.parser }
            let w = img.width, h = img.height
            var matte = [Float](repeating: 0, count: w * h), other = matte
            for i in 0..<(w * h) {
                if i % w >= w / 4, i % w < 3 * w / 4 { matte[i] = 1 }
                if i % w < w / 8 { other[i] = 1 }
            }
            let mattes = persons == 2 ? [matte, other] : [matte]
            return PeopleObservation(width: img.width, height: img.height, mattes: mattes,
                                     faces: [FaceHint(box: CGRect(x: w / 2 - 6, y: 6, width: 12, height: 12), angle: 0)])
        }, parser: { Parser() }, cacheLimit: 1)
    }

    @Test func serviceCachesPerImageAndCombinesPeople() async throws {
        let c = Counter()
        let s = Self.service(c)
        let img = Self.image()
        let m = try await s.masks(for: img)
        _ = try await s.masks(for: img)
        #expect(c.n == 1 && s.computed == 1 && m.persons.count == 2)
        let all = try #require(m.all[.person])
        #expect(all.bytes[0] == 255 && all.bytes[15] == 255 && all.bytes[7] == 0, "all people = max over persons")
        #expect(m.mask(.person, person: 0)?.bytes[15] == 255 && m.mask(.person, person: 1)?.bytes[0] == 255)
        #expect(m.mask(.person, person: 1)?.bytes[15] == 0 && m.mask(.hair, person: 5) == nil && m.mask(.face, person: -1) == nil)
        #expect(try await s.mask(.face, person: 0, image: img) != nil)
        #expect(try await s.personCount(for: img) == 2 && c.n == 1)
        // Another image evicts the first (limit 1).
        let other = Self.image()
        #expect(try await s.personCount(for: other) == 2 && c.n == 2)
        #expect(try await s.personCount(for: other) == 2 && c.n == 2)
        _ = try await s.masks(for: other)
        _ = try await s.masks(for: img)
        #expect(c.n == 4)
        let failing = Self.service(Counter(), fail: true)
        await #expect(throws: PeopleMaskError.parser) { try await failing.masks(for: img) }
        let none = PeopleMasks(width: 1, height: 1, parts: [])
        #expect(none.all.isEmpty && none.mask(.hair, person: nil) == nil)
    }

    @Test func maskSourcesAnswerPerPersonCalls() {
        let a = VisionMask(width: 2, height: 1, bytes: [255, 0])
        let b = VisionMask(width: 2, height: 1, bytes: [0, 255])
        let s = StaticMasks([.hair: a], persons: [PersonMaskKey(target: .hair, person: 1): b])
        #expect(s.aiMask(target: .hair)?.data == Data([255, 0]))
        #expect(s.aiPersonMask(target: .hair, person: 1)?.data == Data([0, 255]))
        #expect(s.aiPersonMask(target: .hair, person: 1)?.version == b.version)
        #expect(s.aiPersonMask(target: .hair, person: 0) == nil && s.aiPersonMask(target: .face, person: 1) == nil)
        let e = ExportMasks([7: [.sky: a]], persons: [8: [PersonMaskKey(target: .teeth, person: 0): b]])
        #expect(e.aiMask(sourceId: 7, target: .sky)?.width == 2)
        #expect(e.aiPersonMask(sourceId: 8, target: .teeth, person: 0)?.data == Data([0, 255]))
        #expect(e.aiPersonMask(sourceId: 7, target: .teeth, person: 0) == nil && e.aiPersonMask(sourceId: 9, target: .teeth, person: 0) == nil)
        let pairs: [(DevelopAiTarget, AITarget)] = [(.subject, .subject), (.sky, .sky), (.person, .person), (.background, .background),
                                                    (.face, .face), (.skin, .skin), (.eyes, .eyes), (.teeth, .teeth),
                                                    (.hair, .hair), (.clothes, .clothes)]
        for (d, t) in pairs { #expect(AITarget(d) == t) }
    }

    @Test func gatherCollectsWholeImageAndPerPersonMasks() async throws {
        var edit = EditState()
        edit.layers = [LayerEdit(id: 1, name: "a", mask: MaskEdit(shapes: [.ai(.add, .hair), .aiPerson(.add, .face, person: 1)])),
                       LayerEdit(id: 2, name: "off", enabled: false, mask: MaskEdit(shapes: [.aiPerson(.add, .eyes, person: 0)]))]
        #expect(DevelopRenderer.personKeys(in: edit) == [PersonMaskKey(target: .face, person: 1)])
        let mask = VisionMask(width: 1, height: 1, bytes: [255])
        let source = await StaticMasks.gather(edit, photo: PhotoID(1), provider: { _, t in
            Dictionary(uniqueKeysWithValues: t.map { ($0, mask) })
        }, persons: { _, keys in Dictionary(uniqueKeysWithValues: keys.map { ($0, mask) }) })
        #expect(source?.masks.keys.sorted { $0.rawValue < $1.rawValue } == [.hair])
        #expect(source?.persons.keys.first == PersonMaskKey(target: .face, person: 1))
        #expect(await StaticMasks.gather(edit, photo: PhotoID(1), provider: nil, persons: nil) == nil)
        #expect(await StaticMasks.gather(EditState(), photo: PhotoID(1), provider: { _, _ in [:] }, persons: { _, _ in [:] }) == nil)
        let onlyPersons = await StaticMasks.gather(edit, photo: PhotoID(1), provider: nil, persons: { _, _ in [:] })
        #expect(onlyPersons != nil && onlyPersons?.masks.isEmpty == true)

        var export = RustExportService()
        let item = ExportJobItem(photoID: PhotoID(3), url: URL(fileURLWithPath: "/tmp/DSC1.ARW"), seq: 1, edit: edit)
        #expect(await export.gatherPersonMasks([1: item]).isEmpty, "no provider")
        export.maskProvider = { _, t in Dictionary(uniqueKeysWithValues: t.map { ($0, mask) }) }
        export.personMaskProvider = { _, keys in Dictionary(uniqueKeysWithValues: keys.map { ($0, mask) }) }
        let g = await export.gatherMasks([1: item, 2: ExportJobItem(photoID: PhotoID(4), url: URL(fileURLWithPath: "/tmp/b"), seq: 2, edit: nil)])
        #expect(g.missing.isEmpty)
        #expect(g.masks?.aiPersonMask(sourceId: 1, target: .face, person: 1) != nil)
        #expect(g.masks?.aiMask(sourceId: 1, target: .hair) != nil)
        var personsOnly = EditState()
        personsOnly.layers = [LayerEdit(id: 1, name: "p", mask: MaskEdit(shapes: [.aiPerson(.add, .skin, person: 0)]))]
        let g2 = await export.gatherMasks([5: ExportJobItem(photoID: PhotoID(3), url: URL(fileURLWithPath: "/tmp/c"), seq: 1, edit: personsOnly)])
        #expect(g2.masks?.aiPersonMask(sourceId: 5, target: .skin, person: 0) != nil)

        // A per-person mask the people model couldn't make is reported (the photo isn't
        // exported with an empty layer), and the message names the target.
        export.personMaskProvider = { _, _ in [:] }
        let g3 = await export.gatherMasks([5: ExportJobItem(photoID: PhotoID(3), url: URL(fileURLWithPath: "/tmp/c"), seq: 1, edit: personsOnly)])
        #expect(g3.missing == [5: [.skin]])
        let msg = RustExportService.maskFailure(item, targets: [.skin])
        #expect(msg.contains("DSC1") && msg.contains("skin"))
    }

    @Test func viewerPersonMasksAndThePanelActions() async throws {
        let m = makeModel()
        await m.start()
        let img = Self.image()
        let s = Self.service(Counter())
        let got = await ViewerModel.personMasks([PersonMaskKey(target: .clothes, person: 0), PersonMaskKey(target: .hair, person: 9)],
                                                image: img, service: s)
        #expect(got.keys.map(\.person) == [0])
        #expect(await ViewerModel.personMasks([PersonMaskKey(target: .hair, person: 0)], image: img,
                                              service: Self.service(Counter(), fail: true)).isEmpty)
        // The focused photo through the app and the viewer (one fake service: one pass).
        let svc = Self.service(Counter())
        m.viewer.peopleService = svc
        let layer = try await m.addPeopleMask(.clothes, person: 1, toLayer: nil)
        #expect(m.focusedEdit.layers.first { $0.id == layer }?.mask.shapes == [.aiPerson(.add, .clothes, person: 1)])
        #expect(m.focusedEdit.layers.last?.name == "Clothes (Person 2)")
        _ = try await m.addPeopleMask(.skin, person: nil, toLayer: layer)
        #expect(m.focusedEdit.layers.count == 1 && m.viewer.masks.mask(photo: m.focusedID!, target: .skin) != nil)
        await #expect(throws: AIMaskError.nothingFound(.teeth)) { try await m.addPeopleMask(.teeth, person: 0, toLayer: nil) }
        let lc = LayerController(app: m)
        try await lc.addPeopleMask(.person, person: 0)
        #expect(lc.selected != nil && m.focusedEdit.layers.last?.mask.shapes.last == .aiPerson(.add, .person, person: 0))
        #expect(await lc.peopleCount() == 2)
        #expect(await m.viewer.peopleCount() == 2)
        #expect(await m.viewer.personMasksForRender(photo: m.focusedID!, keys: [PersonMaskKey(target: .person, person: 1)]).count == 1)
        #expect(await m.viewer.personMasksForRender(photo: PhotoID(999_999), keys: []).isEmpty)
        #expect(svc.computed == 1, "every call shares one pass on the photo")
        m.focus(nil, extend: false)
        await #expect(throws: AIMaskError.noImage) { try await m.addPeopleMask(.hair, person: nil, toLayer: nil) }
        let n1 = await lc.peopleCount(), n2 = await m.viewer.peopleCount()
        #expect(n1 == 0 && n2 == 0)
    }

    @Test func peopleBoxesAndMatching() {
        let m = PeopleMasks(width: 4, height: 2, parts: [PersonParts(masks: [.person: [0, 255, 255, 0, 0, 255, 0, 0]]),
                                                         PersonParts(masks: [.hair: [255, 0, 0, 0, 0, 0, 0, 0]])])
        #expect(m.boxes == [CGRect(x: 0.25, y: 0, width: 0.5, height: 1), .zero])
        #expect(PeopleMasks.box(of: [UInt8](repeating: 10, count: 4), width: 2, height: 2) == .zero)
        let a = CGRect(x: 0, y: 0, width: 0.4, height: 1), b = CGRect(x: 0.5, y: 0, width: 0.5, height: 1)
        #expect(PersonMatch.overlap(a, a) == 1 && PersonMatch.overlap(a, b) == 0)
        #expect(abs(PersonMatch.overlap(a, CGRect(x: 0, y: 0, width: 0.2, height: 1)) - 0.5) < 1e-9)
        // Today's order is the other way round; person 2 is gone.
        let stored: [Int: CGRect] = [0: a, 1: b, 2: CGRect(x: 0.9, y: 0.9, width: 0.05, height: 0.05)]
        let now = [b.insetBy(dx: 0.01, dy: 0.01), a]
        #expect(PersonMatch.map(stored: stored, current: now) == [0: 1, 1: 0])
        #expect(PersonMatch.resolve(0, stored: stored, current: now) == 1)
        #expect(PersonMatch.resolve(2, stored: stored, current: now) == nil, "not found any more")
        #expect(PersonMatch.resolve(5, stored: stored, current: now) == 5, "no box: the index as it was")
        // One current person for one stored person: the better overlap wins.
        let twins: [Int: CGRect] = [0: a, 1: a.insetBy(dx: 0.05, dy: 0.05)]
        #expect(PersonMatch.map(stored: twins, current: [a]) == [0: 0])
    }

    /// The edit's people renumbered to today's order before a new person is added, so an
    /// index never means two people; the lost ones move past today's people.
    @Test func adoptingThePeopleOrder() {
        let a = CGRect(x: 0, y: 0, width: 0.4, height: 1), b = CGRect(x: 0.5, y: 0, width: 0.5, height: 1)
        let gone = CGRect(x: 0.9, y: 0.9, width: 0.05, height: 0.05)
        var e = EditState()
        e.layers = [LayerEdit(id: 1, name: "x", mask: MaskEdit(shapes: [.aiPerson(.add, .hair, person: 0),
                                                                    .aiPerson(.add, .face, person: 1),
                                                                    .aiPerson(.add, .eyes, person: 2),
                                                                    .aiPerson(.add, .skin, person: 7), .ai(.add, .sky)]))]
        e.personBoxes = [0: a, 1: b, 2: gone]
        e.adoptPeopleOrder([b, a])
        #expect(e.layers[0].mask.shapes == [.aiPerson(.add, .hair, person: 1), .aiPerson(.add, .face, person: 0),
                                            .aiPerson(.add, .eyes, person: 2), .aiPerson(.add, .skin, person: 7),
                                            .ai(.add, .sky)])
        #expect(e.personBoxes == [1: a, 0: b, 2: gone])
        let same = e
        e.adoptPeopleOrder([b, a])
        #expect(e == same, "already in order")
        var none = EditState()
        none.adoptPeopleOrder([a])
        #expect(none == EditState())
    }

    /// Parser-less (colour model) results aren't kept once the parser is there, and a
    /// result made without high-resolution face renders is made again when they can be.
    @Test func fallbackAndLowResolutionResultsAreRedone() async throws {
        final class Flags: @unchecked Sendable {
            let lock = NSLock()
            var parser = false
            var regions: [(CGRect, Int)] = []
            var firstPixel: [UInt8] = []
        }
        let flags = Flags()
        struct Big: FaceParser {
            let flags: Flags
            let inputSize = 64
            func parse(rgba: [UInt8]) throws -> [UInt8] {
                flags.lock.withLock { flags.firstPixel = Array(rgba[(32 * 64 + 32) * 4..<(32 * 64 + 32) * 4 + 3]) }
                return [UInt8](repeating: FaceParsingClass.skin.rawValue, count: 64 * 64)
            }
        }
        let c = Counter()
        let s = PeopleMaskService(observe: { img in
            c.lock.withLock { c.n += 1 }
            let w = img.width, h = img.height
            return PeopleObservation(width: w, height: h, mattes: [[Float](repeating: 1, count: w * h)],
                                     faces: [FaceHint(box: CGRect(x: 14, y: 14, width: 12, height: 12), angle: 0)])
        }, parser: { flags.lock.withLock { flags.parser } ? Big(flags: flags) : nil },
           parserReady: { flags.lock.withLock { flags.parser } })
        let img = Self.image()
        let fallback = try await s.masks(for: img)
        #expect(!fallback.parsed && c.n == 1)
        _ = try await s.masks(for: img)
        #expect(c.n == 1, "kept while the parser isn't there")
        flags.lock.withLock { flags.parser = true }
        let parsed = try await s.masks(for: img)
        #expect(parsed.parsed && !parsed.detailed && c.n == 2, "made again with the parser")
        // A 12 px face for a 64 px parser: the face is rendered at high resolution.
        let red: PeopleMaskService.FaceDetail = { region, long in
            flags.lock.withLock { flags.regions.append((region, long)) }
            let ctx = CGContext(data: nil, width: 64, height: 64, bitsPerComponent: 8, bytesPerRow: 0,
                                space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
            ctx.setFillColor(CGColor(srgbRed: 1, green: 0, blue: 0, alpha: 1))
            ctx.fill(CGRect(x: 0, y: 0, width: 64, height: 64))
            return (ctx.makeImage()!, region)
        }
        let sharp = try await s.masks(for: img, detail: red)
        #expect(sharp.detailed && c.n == 3)
        let asked = try #require(flags.lock.withLock { flags.regions.first })
        #expect(asked.1 > 50 && asked.0.minX > 0 && asked.0.maxX < 1, "the face's region, at more than the analysis size")
        #expect(flags.lock.withLock { flags.firstPixel } == [255, 0, 0], "parsed on the render")
        _ = try await s.masks(for: img, detail: red)
        _ = try await s.masks(for: img)
        #expect(c.n == 3, "a detailed result serves everyone")
        // A render that fails: the analysis image, and not counted as detailed.
        let other = Self.image()
        let failed = try await s.masks(for: other, detail: { _, _ in nil })
        #expect(!failed.detailed)
        let n = c.lock.withLock { c.n }
        _ = try await s.masks(for: other, detail: { _, _ in nil })
        _ = try await s.masks(for: other, detail: red)
        #expect(c.lock.withLock { c.n } == n, "a failed detail render isn't retried on every call")
        // Failed in the background: retried once when the photo is on screen, then kept.
        let third = Self.image()
        _ = try await s.masks(for: third, detail: { _, _ in nil }, interactive: false)
        _ = try await s.masks(for: third, detail: { _, _ in nil }, interactive: false)
        #expect(c.lock.withLock { c.n } == n + 1, "not retried in the background")
        let focused = try await s.masks(for: third, detail: red)
        #expect(focused.detailed && c.lock.withLock { c.n } == n + 2, "retried interactively")
        _ = try await s.masks(for: third, detail: red)
        #expect(c.lock.withLock { c.n } == n + 2)
        // Small parsers need no render.
        let small = FaceCrop.around(face: CGRect(x: 14, y: 14, width: 12, height: 12), angle: 0, size: 8)
        let px = PixelImage(width: 40, height: 40, rgba: [UInt8](repeating: 9, count: 40 * 40 * 4))
        let direct = await PeopleMaskService.faceInput(small, pixels: px, detail: { _, _ in Issue.record("rendered"); return nil })
        #expect(direct.sharp && direct.rgba.count == 8 * 8 * 4)
    }

    /// Per-person masks follow the person by the box the edit stored; a person no longer
    /// found renders empty and is reported once on the photo on screen.
    @Test func personMasksFindTheirPeopleAgain() async throws {
        let img = Self.image()
        let s = Self.service(Counter())
        let now = try await s.masks(for: img)
        // The edit's person 0 is today's person 1, its person 3 is gone.
        let boxes: [Int: CGRect] = [0: now.boxes[1], 3: CGRect(x: 0.9, y: 0.9, width: 0.05, height: 0.05)]
        let got = await ViewerModel.personMasks([PersonMaskKey(target: .person, person: 0), PersonMaskKey(target: .person, person: 3),
                                                 PersonMaskKey(target: .person, person: 1)],
                                                image: img, service: s, boxes: boxes)
        #expect(got[PersonMaskKey(target: .person, person: 0)] == now.mask(.person, person: 1))
        #expect(got[PersonMaskKey(target: .person, person: 1)] == now.mask(.person, person: 1), "no box: as indexed")
        #expect(got[PersonMaskKey(target: .person, person: 3)] == nil)

        let m = makeModel()
        await m.start()
        m.viewer.peopleService = Self.service(Counter())
        let layer = try await m.addPeopleMask(.clothes, person: 1, toLayer: nil)
        let id = try #require(m.focusedID)
        let stored = try #require(m.focusedEdit.personBoxes[1], "the person's box is stored with the shape")
        #expect(PersonMatch.overlap(stored, now.boxes[1]) > 0.9)
        let json = try AdjustmentsCodec.encode(m.focusedEdit)
        #expect(json.contains("person_boxes"))
        m.updateEdits([id], label: "x") { _, e in e.personBoxes[1] = CGRect(x: 0.9, y: 0.9, width: 0.05, height: 0.05) }
        let lost = await m.viewer.personMasksForRender(photo: id, keys: [PersonMaskKey(target: .clothes, person: 1)])
        #expect(lost.isEmpty && m.toast?.message == "Person 2 isn't found in this photo any more: those people masks are empty.")
        m.toast = nil
        _ = await m.viewer.personMasksForRender(photo: id, keys: [PersonMaskKey(target: .clothes, person: 1)])
        #expect(m.toast == nil, "reported once")
        #expect(m.focusedEdit.layers.contains { $0.id == layer })
        #expect(m.viewer.faceDetail(for: try #require(m.focusedPhoto)) == nil, "the fake renderer has no region renders")
        let msg = RustExportService.maskFailure(ExportJobItem(photoID: id, url: URL(fileURLWithPath: "/tmp/P.ARW"), seq: 1, edit: nil),
                                                targets: [.hair])
        #expect(msg.contains("isn't found in the photo any more"))
    }

    /// With a renderer that renders regions, faces are rendered at high resolution: the
    /// photo as shot (no edit), the oriented frame, the region asked for.
    @Test func faceDetailRendersTheRegion() async throws {
        let r = ViewerSizedRenderer()
        let m = makeViewerApp(renderer: r)
        await m.start()
        let photo = try #require(m.focusedPhoto)
        let detail = try #require(m.viewer.faceDetail(for: photo))
        let region = CGRect(x: 0.25, y: 0.25, width: 0.25, height: 0.25)
        let got = try #require(await detail(region, 4000))
        #expect(got.image.width > 0 && got.region == region)
        let req = try #require(r.requests.last { $0.region == region })
        #expect(req.maxPixelSize == 4000 && req.geometry == .oriented && req.quality == .full && req.priority == .interactive)
        // Another photo's (a thumbnail's, an export's) queue behind the viewer, unpinned.
        let other = try #require(m.photos.first { $0.id != photo.id })
        let bg = try #require(m.viewer.faceDetail(for: other))
        _ = await bg(region, 4000)
        let bgReq = try #require(r.requests.last { $0.region == region })
        #expect(bgReq.priority == .background)
        #expect(ViewerModel.faceDetailPriority(photo: photo.id, focused: nil) == .background)
    }

    @Test func wiringSetsEveryProvider() {
        let m = makeModel()
        let develop = DevelopRenderer(device: nil)
        m.tools = RustDevelopTools()
        m.exporter = RustExportService()
        m.wirePersonMasks(develop: develop)
        #expect(develop.personMaskProvider != nil)
        #expect((m.tools as? RustDevelopTools)?.personMaskProvider != nil)
        #expect((m.exporter as? RustExportService)?.personMaskProvider != nil)
    }

    @Test func menuLogic() {
        #expect(PeopleMenu.personChoices(count: 2).map(\.label) == ["All People", "Person 1", "Person 2"])
        #expect(PeopleMenu.personChoices(count: -1).count == 1)
        for t in AITarget.allCases { #expect(!PeopleMenu.symbol(t).isEmpty) }
        #expect(PeopleMenu.help(.person, person: nil).contains("every person"))
        #expect(PeopleMenu.help(.hair, person: 0).contains("person 1"))
        #expect(PeopleMenu.downloadStatus == nil)
        PeopleMenu.cancelDownload()
    }
}

/// Sidecar fields: removals and the AI shape's `person`, and Copy/Apply.
struct RemovalCodecTests {
    @Test func removalsRoundTripLosslessly() throws {
        let json = """
        {"removals":[{"strokes":[{"points":[[0.1,0.2],[0.3,0.4]],"size":0.03,"hardness":1,"flow":1,"erase":false,"auto_mask":false}],
        "feather":0.004,"future":7}]}
        """
        let (e, raw) = try AdjustmentsCodec.decode(json)
        #expect(e.removals.count == 1 && e.removals[0].feather == 0.004)
        #expect(e.removals[0].strokes[0].points == [CGPoint(x: 0.1, y: 0.2), CGPoint(x: 0.3, y: 0.4)])
        var edit = e
        let back = AdjustmentsCodec.overlay(edit, onto: raw)
        #expect(((back["removals"] as? [[String: Any]])?.first?["future"] as? Int) == 7, "kept as it was")
        edit.removals.append(RemovalEdit(strokes: [BrushStroke(points: [CGPoint(x: 0.5, y: 0.5)], size: 0.1)], feather: 1))
        #expect(edit.removals[1].feather == RemovalEdit.maxFeather)
        let again = AdjustmentsCodec.decode(AdjustmentsCodec.overlay(edit, onto: raw))
        #expect(again.removals == edit.removals)
        #expect(AdjustmentsCodec.decode([:]).removals.isEmpty)
        #expect(RemovalEdit(strokes: [], feather: -1).feather == 0)
    }

    @Test func aiShapesCarryThePerson() throws {
        let shapes: [[String: Any]] = [["kind": "ai", "op": "add", "target": "hair", "person": 2],
                                       ["kind": "ai", "op": "subtract", "target": "teeth"],
                                       ["kind": "ai", "op": "add", "target": "sky", "person": 1],
                                       ["kind": "ai", "op": "add", "target": "nope"]]
        let decoded = shapes.compactMap(AdjustmentsCodec.shape)
        #expect(decoded == [.aiPerson(.add, .hair, person: 2), .ai(.subtract, .teeth), .ai(.add, .sky)])
        let j = AdjustmentsCodec.shapeJSON(.aiPerson(.intersect, .clothes, person: 0))
        #expect(j["person"] as? Int == 0 && j["target"] as? String == "clothes" && j["op"] as? String == "intersect")
        var e = EditState()
        e.layers = [LayerEdit(id: 1, name: "x", mask: MaskEdit(shapes: [.aiPerson(.add, .eyes, person: 3)]))]
        let round = AdjustmentsCodec.decode(AdjustmentsCodec.overlay(e, onto: [:]))
        #expect(round.layers[0].mask.shapes == [.aiPerson(.add, .eyes, person: 3)])
        #expect(LayerOps.maskSummary(round.layers[0].mask) == "Eyes (Person 4)")
        #expect(DevelopRenderer.aiTargets(in: round).isEmpty, "per-person shapes go through the person provider")
    }

    @Test func personBoxesRoundTripOnlyForTheirShapes() throws {
        var e = EditState()
        e.layers = [LayerEdit(id: 1, name: "x", mask: MaskEdit(shapes: [.aiPerson(.add, .eyes, person: 3)]))]
        e.personBoxes = [3: CGRect(x: 0.25, y: 0.5, width: 0.25, height: 0.25), 4: CGRect(x: 0, y: 0, width: 1, height: 1)]
        let json = AdjustmentsCodec.overlay(e, onto: [:])
        let boxes = try #require(json["person_boxes"] as? [String: Any])
        #expect(boxes.keys.sorted() == ["3"] && (boxes["3"] as? [Double]) == [0.25, 0.5, 0.5, 0.75], "only person 3 has shapes")
        let back = AdjustmentsCodec.decode(json)
        #expect(back.personBoxes == [3: CGRect(x: 0.25, y: 0.5, width: 0.25, height: 0.25)])
        #expect(AdjustmentsCodec.overlay(EditState(), onto: ["person_boxes": ["1": [0, 0, 1, 1]]])["person_boxes"] == nil)
        let junk = AdjustmentsCodec.decode(["person_boxes": ["x": [0, 0, 1, 1], "1": [0.5, 0, 0.2, 1], "2": "no", "3": [0, 0, 1, 1]]])
        #expect(junk.personBoxes == [3: CGRect(x: 0, y: 0, width: 1, height: 1)])
        var only = EditState()
        only.personBoxes = [0: .zero]
        #expect(only.isIdentity, "boxes alone aren't an edit")
    }

    @Test func removalsTravelWithHealAndClone() {
        var src = EditState()
        src.removals = [RemovalEdit(strokes: [BrushStroke(points: [.zero])])]
        #expect(AdjustGroup.spots.differs(src, EditState()))
        #expect(AdjustGroup.changed(in: src) == [.spots])
        let pasted = AdjustmentClipboard(source: src).apply(to: EditState(), options: PasteOptions(groups: [.spots]))
        #expect(pasted.removals == src.removals)
    }

    @Test func peopleTargets() {
        #expect(PeopleTargets.isPeople(.person) && PeopleTargets.isPeople(.teeth) && !PeopleTargets.isPeople(.sky))
        #expect(PeopleTargets.label(.hair, person: nil) == "Hair" && PeopleTargets.label(.hair, person: 0) == "Hair (Person 1)")
        #expect(PeopleTargets.shape(.hair, person: nil) == .ai(.add, .hair))
        #expect(PeopleTargets.shape(.skin, person: -3, op: .subtract) == .aiPerson(.subtract, .skin, person: 0))
        #expect(PeopleTargets.shape(.subject, person: 1) == .ai(.add, .subject))
        let t = ViewTransform()
        #expect(t.toOriented(.aiPerson(.add, .face, person: 1), displayAspect: 1) == .aiPerson(.add, .face, person: 1))
        #expect(t.toDisplay(.aiPerson(.add, .face, person: 1), displayAspect: 1) == .aiPerson(.add, .face, person: 1))
    }
}
