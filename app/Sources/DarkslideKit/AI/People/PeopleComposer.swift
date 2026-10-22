import CoreGraphics
import DarkslideAI
import Foundation

/// The 19 CelebAMask-HQ classes of the BiSeNet face-parsing model, in its output order
/// (zllrunning/face-parsing.PyTorch `atts`, background first). Checked on the model.
enum FaceParsingClass: UInt8, CaseIterable {
    case background, skin, leftBrow, rightBrow, leftEye, rightEye, eyeGlasses, leftEar, rightEar, earring
    case nose, mouth, upperLip, lowerLip, neck, necklace, cloth, hair, hat

    /// The whole face (skin, brows, eyes, glasses, nose, mouth, lips; not ears or hair).
    static let face: Set<FaceParsingClass> = [.skin, .leftBrow, .rightBrow, .leftEye, .rightEye, .eyeGlasses, .nose,
                                              .mouth, .upperLip, .lowerLip]
    /// Bare skin the parser sees: face skin, nose, ears, neck.
    static let skinParts: Set<FaceParsingClass> = [.skin, .nose, .leftEar, .rightEar, .neck]
    static let eyes: Set<FaceParsingClass> = [.leftEye, .rightEye]
    static let clothes: Set<FaceParsingClass> = [.cloth, .hat]
}

/// A face found by Vision, in E-frame pixels (top-left origin).
struct FaceHint: Equatable, Sendable {
    var box: CGRect
    /// Clockwise turn of the face (radians, `FaceCrop.angle`).
    var angle: Double
    var leftEye: [CGPoint] = []
    var rightEye: [CGPoint] = []
    var innerLips: [CGPoint] = []
}

/// Where a head is when no face was found (body pose: ears, eyes, nose, neck), pixels.
struct HeadHint: Equatable, Sendable {
    var center: CGPoint
    var radius: Double
    /// Facing the camera (a nose or an eye was seen).
    var facing: Bool
    /// Unit vector from the neck to the head, when the neck was seen.
    var up: CGPoint?
}

/// What Vision saw: per person a soft matte (0...1, largest person first), faces and heads.
struct PeopleObservation: Sendable {
    var width: Int
    var height: Int
    var mattes: [[Float]]
    var faces: [FaceHint] = []
    var heads: [HeadHint] = []
}

/// The face parser's class ids for one face crop (`crop.size`² labels).
struct FaceParse: Sendable {
    var crop: FaceCrop
    var labels: [UInt8]
}

/// One person's masks: Person plus the six sub-parts, E-frame, `width × height`.
struct PersonParts: Sendable {
    var masks: [AITarget: [UInt8]]
}

/// Skin or hair colour as a Gaussian in YCbCr (BT.601, 0...255): `probability` is 1
/// near the mean and falls to 0 past `far` standard deviations (Mahalanobis).
struct ColorModel: Equatable, Sendable {
    var mean: SIMD3<Float>
    /// Inverse covariance (row-major 3×3).
    var inverse: [Float]
    var near: Float
    var far: Float
    /// Only chroma (Cb, Cr) counts; luma is free (shadows on skin).
    var chromaOnly: Bool

    static func ycc(_ rgb: SIMD3<Float>) -> SIMD3<Float> {
        let y = 0.299 * rgb.x + 0.587 * rgb.y + 0.114 * rgb.z
        return SIMD3(y, 128 - 0.168736 * rgb.x - 0.331264 * rgb.y + 0.5 * rgb.z,
                     128 + 0.5 * rgb.x - 0.418688 * rgb.y - 0.081312 * rgb.z)
    }

    /// Generic skin: the classic Cb 77–127 / Cr 133–173 box as a Gaussian.
    static let genericSkin = ColorModel(mean: SIMD3(0, 106, 152), inverse: [0, 0, 0, 0, 1.0 / 121, 0, 0, 0, 1.0 / 100] as [Float],
                                        near: 2.0, far: 3.2, chromaOnly: true)

    /// Fitted to `samples` (YCbCr), regularized so a flat patch still gives a usable
    /// spread; nil with fewer than 12 samples.
    static func fit(_ samples: [SIMD3<Float>], chromaOnly: Bool, minSD: Float = 4, near: Float = 2.2,
                    far: Float = 3.6) -> ColorModel? {
        guard samples.count >= 12 else { return nil }
        let n = Float(samples.count)
        let mean = samples.reduce(SIMD3<Float>.zero, +) / n
        var c = [Float](repeating: 0, count: 9)
        for s in samples {
            let d = s - mean
            let v = [d.x, d.y, d.z]
            for i in 0..<3 { for j in 0..<3 { c[i * 3 + j] += v[i] * v[j] } }
        }
        for i in 0..<9 { c[i] /= n }
        for i in 0..<3 { c[i * 4] += minSD * minSD }
        if chromaOnly {
            c[0] = 1; c[1] = 0; c[2] = 0; c[3] = 0; c[6] = 0
        }
        guard let inv = invert3(c) else { return nil }
        var out = ColorModel(mean: mean, inverse: inv, near: near, far: far, chromaOnly: chromaOnly)
        if chromaOnly { out.inverse[0] = 0 }
        return out
    }

    static func invert3(_ m: [Float]) -> [Float]? {
        let a = m[0], b = m[1], c = m[2], d = m[3], e = m[4], f = m[5], g = m[6], h = m[7], i = m[8]
        let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
        guard abs(det) > 1e-9 else { return nil }
        return [(e * i - f * h) / det, (c * h - b * i) / det, (b * f - c * e) / det,
                (f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det,
                (d * h - e * g) / det, (b * g - a * h) / det, (a * e - b * d) / det]
    }

    func distance(_ ycc: SIMD3<Float>) -> Float {
        var v = ycc - mean
        if chromaOnly { v.x = 0 }
        let m = inverse
        let r0 = m[0] * v.x + m[1] * v.y + m[2] * v.z
        let r1 = m[3] * v.x + m[4] * v.y + m[5] * v.z
        let r2 = m[6] * v.x + m[7] * v.y + m[8] * v.z
        return max(0, v.x * r0 + v.y * r1 + v.z * r2).squareRoot()
    }

    func probability(_ ycc: SIMD3<Float>) -> Float {
        1 - PeopleComposer.smoothstep(near, far, distance(ycc))
    }
}

/// Builds each person's Face / Skin / Eyes / Teeth / Hair / Clothes masks from the
/// person mattes, the face parser's classes mapped back into the frame, Vision's face
/// landmarks and colour models (skin calibrated on the person's own face skin).
///
/// Soft edges come from the matte (hair edges included): every part is its class
/// probability times the person's matte.
enum PeopleComposer {
    static func smoothstep(_ a: Float, _ b: Float, _ x: Float) -> Float {
        guard a != b else { return x < a ? 0 : 1 }
        let t = min(max((x - a) / (b - a), 0), 1)
        return t * t * (3 - 2 * t)
    }

    /// Per-pixel class weights of a face parse in the frame (bilinear over the one-hot
    /// labels, so edges are soft), 0...1 per group.
    struct ParseMaps {
        var face: [Float]
        var skin: [Float]
        var eyes: [Float]
        var mouth: [Float]
        var hair: [Float]
        var clothes: [Float]
        /// Pixels the crop covers.
        var covered: [Bool]
    }

    static func project(_ parse: FaceParse, width w: Int, height h: Int) -> ParseMaps {
        var m = ParseMaps(face: .init(repeating: 0, count: w * h), skin: .init(repeating: 0, count: w * h),
                          eyes: .init(repeating: 0, count: w * h), mouth: .init(repeating: 0, count: w * h),
                          hair: .init(repeating: 0, count: w * h), clothes: .init(repeating: 0, count: w * h),
                          covered: .init(repeating: false, count: w * h))
        let n = parse.crop.size
        guard parse.labels.count >= n * n, let b = parse.crop.frameBounds(width: w, height: h) else { return m }
        func label(_ x: Int, _ y: Int) -> FaceParsingClass {
            guard x >= 0, y >= 0, x < n, y < n else { return .background }
            return FaceParsingClass(rawValue: parse.labels[y * n + x]) ?? .background
        }
        for y in b.y0..<b.y1 {
            for x in b.x0..<b.x1 {
                let c = parse.crop.toCrop(CGPoint(x: Double(x) + 0.5, y: Double(y) + 0.5))
                let fx = Double(c.x) - 0.5, fy = Double(c.y) - 0.5
                guard fx > -1, fy > -1, fx < Double(n), fy < Double(n) else { continue }
                let i = y * w + x
                m.covered[i] = true
                let x0 = Int(floor(fx)), y0 = Int(floor(fy))
                let ax = Float(fx - Double(x0)), ay = Float(fy - Double(y0))
                for t in 0..<4 {
                    let tx = x0 + (t & 1), ty = y0 + (t >> 1)
                    let wt = (t & 1 == 0 ? 1 - ax : ax) * (t >> 1 == 0 ? 1 - ay : ay)
                    guard wt > 0 else { continue }
                    let l = label(tx, ty)
                    if FaceParsingClass.face.contains(l) { m.face[i] += wt }
                    if FaceParsingClass.skinParts.contains(l) { m.skin[i] += wt }
                    if FaceParsingClass.eyes.contains(l) { m.eyes[i] += wt }
                    if l == .mouth { m.mouth[i] += wt }
                    if l == .hair { m.hair[i] += wt }
                    if FaceParsingClass.clothes.contains(l) { m.clothes[i] += wt }
                }
            }
        }
        return m
    }

    /// Index of the person whose matte covers `rect` most (mean over a 9×9 grid ≥ 0.2).
    static func owner(of rect: CGRect, mattes: [[Float]], width w: Int, height h: Int) -> Int? {
        var best: (Int, Float)?
        for (k, matte) in mattes.enumerated() {
            var s: Float = 0
            for j in 0..<9 {
                for i in 0..<9 {
                    let x = Int(rect.minX + rect.width * (CGFloat(i) + 0.5) / 9)
                    let y = Int(rect.minY + rect.height * (CGFloat(j) + 0.5) / 9)
                    if x >= 0, y >= 0, x < w, y < h { s += matte[y * w + x] }
                }
            }
            s /= 81
            if s >= 0.2, best == nil || s > best!.1 { best = (k, s) }
        }
        return best?.0
    }

    /// Every person's parts. `parses[i]` is the parse of `observation.faces[i]` (nil when
    /// the parser is unavailable: the face then comes from the landmarks and an ellipse).
    static func compose(image: PixelImage, observation o: PeopleObservation, parses: [FaceParse?]) -> [PersonParts] {
        let w = o.width, h = o.height
        guard image.width == w, image.height == h else { return [] }
        var faceOf: [Int: Int] = [:]
        for (i, f) in o.faces.enumerated() {
            if let k = owner(of: f.box, mattes: o.mattes, width: w, height: h), faceOf[k] == nil { faceOf[k] = i }
        }
        var headOf: [Int: HeadHint] = [:]
        for hint in o.heads {
            let r = CGFloat(hint.radius)
            let box = CGRect(x: hint.center.x - r / 2, y: hint.center.y - r / 2, width: r, height: r)
            if let k = owner(of: box, mattes: o.mattes, width: w, height: h), headOf[k] == nil { headOf[k] = hint }
        }
        return o.mattes.enumerated().map { k, matte in
            let face = faceOf[k].map { o.faces[$0] }
            let parse = faceOf[k].flatMap { $0 < parses.count ? parses[$0] : nil }
            return person(image: image, matte: matte, face: face, parse: parse, head: headOf[k])
        }
    }

    /// One person's parts.
    static func person(image: PixelImage, matte: [Float], face: FaceHint?, parse: FaceParse?, head: HeadHint?) -> PersonParts {
        let w = image.width, h = image.height, n = w * h
        let ycc: [SIMD3<Float>] = (0..<n).map { i in
            ColorModel.ycc(SIMD3(Float(image.rgba[i * 4]), Float(image.rgba[i * 4 + 1]), Float(image.rgba[i * 4 + 2])))
        }
        let maps = parse.map { project($0, width: w, height: h) }
        var pFace = maps?.face ?? [Float](repeating: 0, count: n)
        var eyes = [UInt8](repeating: 0, count: n)
        var mouth = [UInt8](repeating: 0, count: n)
        if let face {
            PolygonRaster.fill(face.leftEye, into: &eyes, width: w, height: h)
            PolygonRaster.fill(face.rightEye, into: &eyes, width: w, height: h)
            PolygonRaster.fill(face.innerLips, into: &mouth, width: w, height: h)
            if maps == nil { pFace = ellipse(face.box, angle: face.angle, width: w, height: h) }
        }

        // Skin: calibrated on this face's skin when the parser saw it, else generic.
        var skinSamples: [SIMD3<Float>] = []
        var hairSamples: [SIMD3<Float>] = []
        if let maps {
            for i in stride(from: 0, to: n, by: 1) where matte[i] > 0.5 {
                if maps.skin[i] > 0.95, maps.face[i] > 0.95 { skinSamples.append(ycc[i]) }
                if maps.hair[i] > 0.95 { hairSamples.append(ycc[i]) }
            }
        }
        let skinModel = ColorModel.fit(skinSamples, chromaOnly: true, minSD: 5) ?? .genericSkin
        let skinY = skinSamples.isEmpty ? Float(150) : skinSamples.reduce(0) { $0 + $1.x } / Float(skinSamples.count)

        var pSkin = [Float](repeating: 0, count: n)
        for i in 0..<n where matte[i] > 0.02 {
            let parsed = maps.map { $0.skin[i] }
            let blocked = maps.map { max($0.hair[i], $0.clothes[i], $0.face[i] - $0.skin[i]) } ?? 0
            let color = skinModel.probability(ycc[i]) * smoothstep(0.15 * skinY, 0.35 * skinY, ycc[i].x)
            pSkin[i] = max(parsed ?? 0, color * (1 - blocked))
        }

        // Hair: the parser's hair, extended by its colour (long hair beyond the crop);
        // without a parse, seeded from the head region (above the face, or the pose).
        var pHair = maps?.hair ?? [Float](repeating: 0, count: n)
        let headRegion: HeadHint? = face.map {
            HeadHint(center: CGPoint(x: $0.box.midX + sin($0.angle) * $0.box.height * 0.35,
                                     y: $0.box.midY - cos($0.angle) * $0.box.height * 0.35),
                     radius: Double(max($0.box.width, $0.box.height)) * 0.8, facing: true)
        } ?? head
        if hairSamples.count < 12, let head = headRegion {
            hairSamples = []
            for i in disk(head.center, head.radius, width: w, height: h)
                where matte[i] > 0.5 && pSkin[i] < 0.3 && pFace[i] < 0.3 {
                hairSamples.append(ycc[i])
            }
            if maps == nil {
                // No parse: what of the head isn't skin is hair (whatever its colour:
                // highlights, a bun); the colour model then follows it down. Brown hair
                // can pass the skin chroma test, so anything much darker than this
                // person's skin counts as hair too.
                var lit: [Float] = []
                for i in 0..<n where matte[i] > 0.8 && pSkin[i] > 0.8 { lit.append(ycc[i].x) }
                lit.sort()
                let bodyY = lit.isEmpty ? skinY : lit[lit.count / 2]
                for i in disk(head.center, head.radius * 1.15, width: w, height: h) where matte[i] > 0.3 {
                    let dark = 1 - smoothstep(0.45 * bodyY, 0.65 * bodyY, ycc[i].x)
                    pHair[i] = max(pHair[i], max(1 - pSkin[i], dark) * (1 - pFace[i]))
                    pSkin[i] *= 1 - dark
                    // Seen from behind, the crown (away from the neck) is all hair.
                    if !head.facing, let up = head.up {
                        let dx = Double(i % w) + 0.5 - Double(head.center.x), dy = Double(i / w) + 0.5 - Double(head.center.y)
                        let along = (dx * Double(up.x) + dy * Double(up.y)) / head.radius
                        let crown = smoothstep(0, 0.25, Float(along))
                        pHair[i] = max(pHair[i], crown * (1 - pFace[i]))
                        pSkin[i] *= 1 - crown
                    }
                }
            }
        }
        if let model = ColorModel.fit(hairSamples, chromaOnly: false, minSD: 6, near: 1.8, far: 3.0), let head = headRegion {
            grow(&pHair, model: model, ycc: ycc, matte: matte, block: pSkin, face: pFace, width: w, height: h,
                 limit: (head.center, head.radius * 4))
        }

        // Face without a facing head (small people): head disk ∩ skin.
        if face == nil, let head, head.facing {
            for i in disk(head.center, head.radius, width: w, height: h) { pFace[i] = max(pFace[i], pSkin[i]) }
        }

        // Teeth: bright, low-chroma pixels inside the mouth (parser's mouth or inner lips).
        var pTeeth = [Float](repeating: 0, count: n)
        for i in 0..<n {
            let region = max(Float(mouth[i]) / 255, maps?.mouth[i] ?? 0)
            guard region > 0 else { continue }
            let c = ycc[i]
            let chroma = hypot(c.y - 128, c.z - 128)
            pTeeth[i] = region * (1 - smoothstep(14, 26, chroma)) * smoothstep(0.7 * skinY, 0.95 * skinY, c.x)
        }
        var pEyes = [Float](repeating: 0, count: n)
        for i in 0..<n { pEyes[i] = max(Float(eyes[i]) / 255, maps?.eyes[i] ?? 0) }

        var out: [AITarget: [UInt8]] = [:]
        func emit(_ t: AITarget, _ p: (Int) -> Float) {
            out[t] = SkySegmentation.soften((0..<n).map { i in UInt8(min(max(p(i) * matte[i], 0), 1) * 255 + 0.5) },
                                            width: w, height: h)
        }
        out[.person] = matte.map { UInt8(min(max($0, 0), 1) * 255 + 0.5) }
        emit(.face) { min(1, pFace[$0]) }
        emit(.skin) { min(1, pSkin[$0]) }
        emit(.hair) { min(1, pHair[$0]) * (1 - min(1, pFace[$0])) }
        emit(.eyes) { min(1, pEyes[$0]) }
        emit(.teeth) { min(1, pTeeth[$0]) }
        emit(.clothes) { i in
            let other = max(min(1, pSkin[i]), min(1, pHair[i]), min(1, pFace[i]))
            return max(1 - other, maps?.clothes[i] ?? 0)
        }
        return PersonParts(masks: out)
    }

    /// Pixel indices inside a circle.
    static func disk(_ c: CGPoint, _ r: Double, width w: Int, height h: Int) -> [Int] {
        guard r > 0 else { return [] }
        var out: [Int] = []
        let x0 = max(0, Int(Double(c.x) - r)), x1 = min(w - 1, Int(Double(c.x) + r))
        let y0 = max(0, Int(Double(c.y) - r)), y1 = min(h - 1, Int(Double(c.y) + r))
        guard x0 <= x1, y0 <= y1 else { return [] }
        for y in y0...y1 {
            for x in x0...x1 where hypot(Double(x) + 0.5 - Double(c.x), Double(y) + 0.5 - Double(c.y)) <= r {
                out.append(y * w + x)
            }
        }
        return out
    }

    /// A soft ellipse filling a face box (turned by `angle`): the face when no parse is available.
    static func ellipse(_ box: CGRect, angle: Double, width w: Int, height h: Int) -> [Float] {
        var out = [Float](repeating: 0, count: w * h)
        let a = Double(box.width) / 2, b = Double(box.height) * 0.6
        let cx = Double(box.midX), cy = Double(box.midY)
        let c = cos(angle), s = sin(angle)
        let r = max(a, b)
        for i in disk(CGPoint(x: cx, y: cy), r * 1.2, width: w, height: h) {
            let dx = Double(i % w) + 0.5 - cx, dy = Double(i / w) + 0.5 - cy
            let u = dx * c + dy * s, v = -dx * s + dy * c
            let d = ((u * u) / (a * a) + (v * v) / (b * b)).squareRoot()
            out[i] = 1 - smoothstep(0.9, 1.05, Float(d))
        }
        return out
    }

    /// Flood-fills hair outward from confident hair pixels through matte pixels of the
    /// hair colour (not skin, not face), within `limit` (centre, radius).
    static func grow(_ hair: inout [Float], model: ColorModel, ycc: [SIMD3<Float>], matte: [Float], block: [Float],
                     face: [Float], width w: Int, height h: Int, limit: (CGPoint, Double)) {
        var queue: [Int] = []
        var seen = [Bool](repeating: false, count: w * h)
        for i in 0..<(w * h) where hair[i] > 0.8 {
            queue.append(i)
            seen[i] = true
        }
        var head = 0
        let r2 = limit.1 * limit.1
        while head < queue.count {
            let i = queue[head]
            head += 1
            let x = i % w, y = i / w
            for t in 0..<4 {
                let nx = x + (t == 0 ? -1 : t == 1 ? 1 : 0), ny = y + (t == 2 ? -1 : t == 3 ? 1 : 0)
                guard nx >= 0, ny >= 0, nx < w, ny < h else { continue }
                let j = ny * w + nx
                guard !seen[j] else { continue }
                seen[j] = true
                let dx = Double(nx) - Double(limit.0.x), dy = Double(ny) - Double(limit.0.y)
                guard dx * dx + dy * dy <= r2, matte[j] > 0.3, block[j] < 0.5, face[j] < 0.5 else { continue }
                let p = model.probability(ycc[j])
                guard p > 0.5 else { continue }
                hair[j] = max(hair[j], p)
                queue.append(j)
            }
        }
    }
}
