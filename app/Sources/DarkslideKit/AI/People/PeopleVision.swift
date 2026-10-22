import CoreGraphics
import CoreML
import CoreVideo
import Foundation
import Vision

/// Apple Vision's people pass on an upright image: person instances (largest first;
/// edges refined with the person-segmentation matte), face landmarks and body pose.
enum PeopleVision {
    static func observe(_ image: CGImage) throws -> PeopleObservation {
        let w = image.width, h = image.height
        let handler = VNImageRequestHandler(cgImage: image)
        let instances = VNGeneratePersonInstanceMaskRequest()
        let segmentation = VNGeneratePersonSegmentationRequest()
        segmentation.qualityLevel = .accurate
        segmentation.outputPixelFormat = kCVPixelFormatType_OneComponent32Float
        let faces = VNDetectFaceLandmarksRequest()
        let pose = VNDetectHumanBodyPoseRequest()
        try handler.perform([instances, segmentation, faces, pose])

        var raw: [[Float]] = []
        if let obs = instances.results?.first {
            for i in obs.allInstances {
                raw.append(scaled(try obs.generateScaledMaskForImage(forInstances: IndexSet(integer: i), from: handler),
                                  width: w, height: h))
            }
        }
        let seg = segmentation.results?.first.map { scaled($0.pixelBuffer, width: w, height: h) }
        let mattes = PeopleMattes.split(instances: raw, segmentation: seg, width: w, height: h)
        return PeopleObservation(width: w, height: h, mattes: mattes,
                                 faces: (faces.results ?? []).map { face($0, width: w, height: h) },
                                 heads: (pose.results ?? []).compactMap { head($0, width: w, height: h) })
    }

    static func face(_ f: VNFaceObservation, width w: Int, height h: Int) -> FaceHint {
        let size = CGSize(width: w, height: h)
        func pts(_ r: VNFaceLandmarkRegion2D?) -> [CGPoint] {
            (r?.pointsInImage(imageSize: size) ?? []).map { VisionGeometry.flipped($0, height: h) }
        }
        let l = pts(f.landmarks?.leftEye), r = pts(f.landmarks?.rightEye)
        let mouth = pts(f.landmarks?.outerLips)
        // Vision's roll is counter-clockwise in its bottom-left frame: clockwise here.
        let angle = FaceCrop.angle(leftEye: l, rightEye: r, mouth: mouth) ?? -(f.roll?.doubleValue ?? 0)
        return FaceHint(box: VisionGeometry.rect(f.boundingBox, width: w, height: h), angle: angle, leftEye: l,
                        rightEye: r, innerLips: pts(f.landmarks?.innerLips))
    }

    static func head(_ b: VNHumanBodyPoseObservation, width w: Int, height h: Int) -> HeadHint? {
        guard let all = try? b.recognizedPoints(.all) else { return nil }
        var named: [String: CGPoint] = [:]
        for (k, p) in all where p.confidence > 0.3 {
            named[k.rawValue.rawValue] = VisionGeometry.point(p.location, width: w, height: h)
        }
        return HeadHint.from(joints: named, imageLong: Double(max(w, h)))
    }

    /// A one-channel mask buffer (8-bit or float) as 0...1 floats at `w`×`h` (nearest
    /// neighbour when Vision's buffer has another size).
    static func scaled(_ buffer: CVPixelBuffer, width w: Int, height h: Int) -> [Float] {
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        let bw = CVPixelBufferGetWidth(buffer), bh = CVPixelBufferGetHeight(buffer)
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        var out = [Float](repeating: 0, count: w * h)
        guard let base = CVPixelBufferGetBaseAddress(buffer), bw > 0, bh > 0 else { return out }
        let isFloat = CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_OneComponent32Float
        for y in 0..<h {
            let row = base.advanced(by: min(bh - 1, y * bh / h) * stride)
            for x in 0..<w {
                let sx = min(bw - 1, x * bw / w)
                out[y * w + x] = isFloat ? min(max(row.load(fromByteOffset: sx * 4, as: Float.self), 0), 1)
                    : Float(row.load(fromByteOffset: sx, as: UInt8.self)) / 255
            }
        }
        return out
    }
}

extension HeadHint {
    /// A head from body-pose joints (pixels, by Vision joint name): the centre of the
    /// head joints seen (nose, eyes, ears), else above the neck; the radius from the ear
    /// span, else the shoulders.
    static func from(joints j: [String: CGPoint], imageLong: Double) -> HeadHint? {
        let headNames = ["nose_joint", "left_eye_joint", "right_eye_joint", "left_ear_joint", "right_ear_joint", "head_joint"]
        let seen = headNames.compactMap { j[$0] }
        let facing = j["nose_joint"] != nil || j["left_eye_joint"] != nil || j["right_eye_joint"] != nil
        var shoulders: Double?
        if let l = j["left_shoulder_1_joint"], let r = j["right_shoulder_1_joint"] {
            shoulders = Double(hypot(l.x - r.x, l.y - r.y))
        }
        var radius = shoulders.map { $0 * 0.33 } ?? imageLong * 0.03
        if let l = j["left_ear_joint"], let r = j["right_ear_joint"] {
            radius = max(Double(hypot(l.x - r.x, l.y - r.y)) * 0.75, radius * 0.5)
        }
        if var c = VisionGeometry.centroid(seen) {
            // Ears and eyes sit below the middle of the skull: move away from the neck.
            var up: CGPoint?
            if let neck = j["neck_1_joint"], hypot(c.x - neck.x, c.y - neck.y) > 1 {
                let d = hypot(c.x - neck.x, c.y - neck.y)
                up = CGPoint(x: (c.x - neck.x) / d, y: (c.y - neck.y) / d)
                c = CGPoint(x: c.x + up!.x * radius * 0.35, y: c.y + up!.y * radius * 0.35)
            }
            return HeadHint(center: c, radius: radius, facing: facing, up: up)
        }
        guard let neck = j["neck_1_joint"] else { return nil }
        // Beyond the neck along the body (hips → neck), so upside-down divers work too;
        // straight up without the hips.
        var dir = CGPoint(x: 0, y: -1)
        let hips = j["root"] ?? VisionGeometry.centroid([j["left_upLeg_joint"], j["right_upLeg_joint"]].compactMap { $0 })
        if let hips, hypot(neck.x - hips.x, neck.y - hips.y) > 1 {
            let d = hypot(neck.x - hips.x, neck.y - hips.y)
            dir = CGPoint(x: (neck.x - hips.x) / d, y: (neck.y - hips.y) / d)
        }
        return HeadHint(center: CGPoint(x: neck.x + dir.x * radius * 1.2, y: neck.y + dir.y * radius * 1.2), radius: radius,
                        facing: false, up: dir)
    }
}

/// Person mattes: Vision's instances give identity, the person-segmentation matte the
/// edge detail (hair). With no instances, the segmentation's connected parts.
enum PeopleMattes {
    /// Mattes largest first: `seg × W_k / max(1, ΣW)` with `W_k` the instance spread a
    /// little (box blur, doubled) so fine hair the instance mask misses still belongs to
    /// its person. Without a segmentation the instances are used as they are.
    static func split(instances: [[Float]], segmentation seg: [Float]?, width w: Int, height h: Int) -> [[Float]] {
        var inst = instances
        if let seg {
            // People the instance request missed (small, partly hidden): the segmentation's
            // parts no instance covers.
            let covered = inst.isEmpty ? nil : inst.dropFirst().reduce(inst[0]) { a, b in zip(a, b).map { max($0, $1) } }
            for c in components(seg, width: w, height: h) {
                guard let covered else { inst.append(c); continue }
                var area: Float = 0, over: Float = 0
                for i in 0..<(w * h) where c[i] > 0 {
                    area += 1
                    over += covered[i]
                }
                if over < 0.1 * area { inst.append(c) }
            }
        }
        guard !inst.isEmpty else { return [] }
        var out: [[Float]]
        if let seg {
            let r = max(1, max(w, h) / 100)
            let spread = inst.map { boxBlur($0, radius: r, width: w, height: h).map { min(1, $0 * 2) } }
            out = spread.map { _ in [Float](repeating: 0, count: w * h) }
            for i in 0..<(w * h) {
                var total: Float = 0
                for s in spread { total += s[i] }
                guard total > 0 else { continue }
                let k = max(1, total)
                for (p, s) in spread.enumerated() { out[p][i] = max(seg[i] * s[i] / k, min(inst[p][i], seg[i])) }
            }
        } else {
            out = inst
        }
        return out.map { ($0, $0.reduce(0, +)) }.sorted { $0.1 > $1.1 }.map(\.0)
    }

    /// Connected parts (4-neighbour) of `matte > 0.5` larger than 0.02 % of the frame, as
    /// binary mattes.
    static func components(_ matte: [Float], width w: Int, height h: Int) -> [[Float]] {
        var label = [Int](repeating: -1, count: w * h)
        var out: [[Float]] = []
        let minArea = max(4, w * h / 5000)
        for start in 0..<(w * h) where matte[start] > 0.5 && label[start] < 0 {
            var stack = [start]
            var members: [Int] = []
            label[start] = out.count
            while let i = stack.popLast() {
                members.append(i)
                let x = i % w, y = i / w
                for t in 0..<4 {
                    let nx = x + (t == 0 ? -1 : t == 1 ? 1 : 0), ny = y + (t == 2 ? -1 : t == 3 ? 1 : 0)
                    guard nx >= 0, ny >= 0, nx < w, ny < h else { continue }
                    let j = ny * w + nx
                    if matte[j] > 0.5, label[j] < 0 {
                        label[j] = out.count
                        stack.append(j)
                    }
                }
            }
            guard members.count >= minArea else { continue }
            var m = [Float](repeating: 0, count: w * h)
            for i in members { m[i] = 1 }
            out.append(m)
        }
        return out
    }

    /// Separable box blur (window `2r+1`, shrinking at the edges).
    static func boxBlur(_ m: [Float], radius r: Int, width w: Int, height h: Int) -> [Float] {
        var tmp = [Float](repeating: 0, count: w * h)
        var out = [Float](repeating: 0, count: w * h)
        for y in 0..<h {
            var prefix = [Float](repeating: 0, count: w + 1)
            for x in 0..<w { prefix[x + 1] = prefix[x] + m[y * w + x] }
            for x in 0..<w {
                let a = max(0, x - r), b = min(w, x + r + 1)
                tmp[y * w + x] = (prefix[b] - prefix[a]) / Float(b - a)
            }
        }
        for x in 0..<w {
            var prefix = [Float](repeating: 0, count: h + 1)
            for y in 0..<h { prefix[y + 1] = prefix[y] + tmp[y * w + x] }
            for y in 0..<h {
                let a = max(0, y - r), b = min(h, y + r + 1)
                out[y * w + x] = (prefix[b] - prefix[a]) / Float(b - a)
            }
        }
        return out
    }
}

/// Face parsing on one upright crop: class ids (`FaceParsingClass`) per pixel.
protocol FaceParser: Sendable {
    var inputSize: Int { get }
    func parse(rgba: [UInt8]) throws -> [UInt8]
}

/// The BiSeNet Core ML model: `input` (colour 512×512) → `argmax_out` (class ids).
struct CoreMLFaceParser: FaceParser, @unchecked Sendable {
    let model: MLModel
    let inputSize = 512

    func parse(rgba: [UInt8]) throws -> [UInt8] {
        let buffer = try PixelBuffers.bgra(rgba: rgba, width: inputSize, height: inputSize)
        let out = try model.prediction(from: MLDictionaryFeatureProvider(dictionary: ["input": MLFeatureValue(pixelBuffer: buffer)]))
        guard let a = out.featureValue(for: "argmax_out")?.multiArrayValue else { throw PeopleMaskError.parser }
        return try Self.labels(a, count: inputSize * inputSize)
    }

    /// The first `n` class ids of `a` in logical (row-major) order, read through its
    /// strides (Core ML may pad rows). Other element types than Float32/16, Int32 and
    /// Double, or fewer than `n` elements, are an error (never misread memory).
    static func labels(_ a: MLMultiArray, count n: Int) throws -> [UInt8] {
        guard a.count >= n else { throw PeopleMaskError.parser }
        let shape = a.shape.map(\.intValue), strides = a.strides.map(\.intValue)
        // Element offset of logical index i.
        func offset(_ i: Int) -> Int {
            var rest = i, off = 0
            for k in stride(from: shape.count - 1, through: 0, by: -1) {
                off += (rest % shape[k]) * strides[k]
                rest /= shape[k]
            }
            return off
        }
        let dense = strides == Self.denseStrides(shape)
        var out = [UInt8](repeating: 0, count: n)
        func read<T>(_: T.Type, _ value: (T) -> Double) {
            let p = a.dataPointer.assumingMemoryBound(to: T.self)
            for i in 0..<n {
                // A non-finite id (a model's numeric failure) is background, never a crash.
                let v = value(p[dense ? i : offset(i)])
                out[i] = v.isFinite ? UInt8(min(max(v.rounded(), 0), 18)) : 0
            }
        }
        switch a.dataType {
        case .float32: read(Float32.self) { Double($0) }
        case .float16: read(Float16.self) { Double($0) }
        case .int32: read(Int32.self) { Double($0) }
        case .double: read(Double.self) { $0 }
        default: throw PeopleMaskError.parser
        }
        return out
    }

    /// Row-major strides of a packed array of `shape`.
    static func denseStrides(_ shape: [Int]) -> [Int] {
        var out = [Int](repeating: 1, count: shape.count)
        for k in stride(from: shape.count - 2, through: 0, by: -1) { out[k] = out[k + 1] * shape[k + 1] }
        return out
    }
}

enum PeopleMaskError: Error, Equatable {
    case parser
    case pixelBuffer
}

/// CVPixelBuffers for Core ML image inputs and outputs.
enum PixelBuffers {
    /// 32BGRA from RGBA (alpha ignored).
    static func bgra(rgba: [UInt8], width w: Int, height h: Int) throws -> CVPixelBuffer {
        var pb: CVPixelBuffer?
        let attrs = [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
        guard CVPixelBufferCreate(nil, w, h, kCVPixelFormatType_32BGRA, attrs, &pb) == kCVReturnSuccess, let pb else {
            throw PeopleMaskError.pixelBuffer
        }
        CVPixelBufferLockBaseAddress(pb, [])
        defer { CVPixelBufferUnlockBaseAddress(pb, []) }
        let base = CVPixelBufferGetBaseAddress(pb)!.assumingMemoryBound(to: UInt8.self)
        let stride = CVPixelBufferGetBytesPerRow(pb)
        for y in 0..<h {
            for x in 0..<w {
                let s = (y * w + x) * 4, d = y * stride + x * 4
                base[d] = rgba[s + 2]
                base[d + 1] = rgba[s + 1]
                base[d + 2] = rgba[s]
                base[d + 3] = 255
            }
        }
        return pb
    }

    /// OneComponent8 from bytes.
    static func gray(_ bytes: [UInt8], width w: Int, height h: Int) throws -> CVPixelBuffer {
        var pb: CVPixelBuffer?
        let attrs = [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
        guard CVPixelBufferCreate(nil, w, h, kCVPixelFormatType_OneComponent8, attrs, &pb) == kCVReturnSuccess, let pb else {
            throw PeopleMaskError.pixelBuffer
        }
        CVPixelBufferLockBaseAddress(pb, [])
        defer { CVPixelBufferUnlockBaseAddress(pb, []) }
        let base = CVPixelBufferGetBaseAddress(pb)!.assumingMemoryBound(to: UInt8.self)
        let stride = CVPixelBufferGetBytesPerRow(pb)
        for y in 0..<h { for x in 0..<w { base[y * stride + x] = bytes[y * w + x] } }
        return pb
    }

    /// RGB bytes (w×h×3) of a 32BGRA or 32ARGB buffer.
    static func rgb(_ pb: CVPixelBuffer) -> (width: Int, height: Int, rgb: [UInt8]) {
        CVPixelBufferLockBaseAddress(pb, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
        let stride = CVPixelBufferGetBytesPerRow(pb)
        var out = [UInt8](repeating: 0, count: w * h * 3)
        guard let raw = CVPixelBufferGetBaseAddress(pb) else { return (w, h, out) }
        let base = raw.assumingMemoryBound(to: UInt8.self)
        let argb = CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32ARGB
        for y in 0..<h {
            for x in 0..<w {
                let s = y * stride + x * 4, d = (y * w + x) * 3
                if argb {
                    out[d] = base[s + 1]; out[d + 1] = base[s + 2]; out[d + 2] = base[s + 3]
                } else {
                    out[d] = base[s + 2]; out[d + 1] = base[s + 1]; out[d + 2] = base[s]
                }
            }
        }
        return (w, h, out)
    }
}
