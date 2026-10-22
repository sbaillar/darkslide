import CoreML
import CoreVideo
import DarkslideFFI
import Foundation

/// Runs an inpainting model on a `side × side` square: RGB (row-major, 3 bytes per
/// pixel) and a mask (255 = fill) in, RGB out. Seam for tests.
protocol InpaintModel: Sendable {
    var side: Int { get }
    func predict(rgb: [UInt8], mask: [UInt8]) throws -> [UInt8]
}

/// The Core ML LaMa package: `image` (colour) + `mask` (grayscale, 255 = hole) → `output`.
struct CoreMLInpaintModel: InpaintModel, @unchecked Sendable {
    let model: MLModel
    let side: Int

    init(model: MLModel, side: Int = 800) {
        self.model = model
        self.side = side
    }

    func predict(rgb: [UInt8], mask: [UInt8]) throws -> [UInt8] {
        var rgba = [UInt8](repeating: 255, count: side * side * 4)
        for i in 0..<(side * side) {
            rgba[i * 4] = rgb[i * 3]
            rgba[i * 4 + 1] = rgb[i * 3 + 1]
            rgba[i * 4 + 2] = rgb[i * 3 + 2]
        }
        let input = try MLDictionaryFeatureProvider(dictionary: [
            "image": MLFeatureValue(pixelBuffer: try PixelBuffers.bgra(rgba: rgba, width: side, height: side)),
            "mask": MLFeatureValue(pixelBuffer: try PixelBuffers.gray(mask, width: side, height: side)),
        ])
        let out = try model.prediction(from: input)
        guard let pb = out.featureValue(for: "output")?.imageBufferValue else { throw InpaintError.noOutput }
        let r = PixelBuffers.rgb(pb)
        guard r.width == side, r.height == side else { throw InpaintError.noOutput }
        return r.rgb
    }
}

enum InpaintError: Error, Equatable {
    case noOutput
}

/// Removal fills with LaMa (ds-ffi `DevelopInpainter`). Rust hands a crop around the
/// hole (at most `maxEdge` on its long side); it is fitted into the model's square
/// (aspect kept, the rest mirrored), filled, and resized back. Only hole pixels change:
/// every known pixel comes back exactly as it went in. Thread-safe (predictions are
/// serialized: one 800×800 LaMa at a time is the memory budget).
final class LamaInpainter: DevelopInpainter, @unchecked Sendable {
    let model: any InpaintModel
    let id: String
    private let lock = NSLock()

    init(model: any InpaintModel, id: String = LamaInpainter.modelID) {
        self.model = model
        self.id = id
    }

    /// The model and its pinned revision (part of every fill's cache key).
    static let modelID = "lama-" + String(DownloadableModelDescriptor.lamaRevision.prefix(7))

    func modelId() -> String { id }
    func maxEdge() -> UInt32 { UInt32(model.side) }

    func inpaint(width: UInt32, height: UInt32, rgb: Data, mask: Data) -> Data? {
        let w = Int(width), h = Int(height)
        guard w > 0, h > 0, rgb.count == w * h * 3, mask.count == w * h else { return nil }
        let src = [UInt8](rgb), holes = [UInt8](mask)
        guard holes.contains(where: { $0 > 0 }) else { return rgb }
        let fit = InpaintFit(width: w, height: h, side: model.side)
        let filled: [UInt8]
        do {
            filled = try lock.withLock { try model.predict(rgb: fit.squareRGB(src), mask: fit.squareMask(holes)) }
        } catch {
            return nil
        }
        guard filled.count == model.side * model.side * 3 else { return nil }
        return Data(fit.composite(original: src, mask: holes, filled: filled))
    }
}

/// Geometry of fitting a `width × height` crop into the model's `side × side` square:
/// scaled uniformly so the long edge is `side`, placed at the top-left, the remainder
/// mirrored (no hole there).
struct InpaintFit: Equatable {
    let width: Int
    let height: Int
    let side: Int

    /// Model pixels per crop pixel.
    var scale: Double { Double(side) / Double(max(width, height)) }
    /// The crop's extent in the square.
    var fittedWidth: Int { min(side, max(1, Int((Double(width) * scale).rounded()))) }
    var fittedHeight: Int { min(side, max(1, Int((Double(height) * scale).rounded()))) }

    /// Mirror index into 0..<n (period 2n).
    static func mirror(_ i: Int, _ n: Int) -> Int {
        guard n > 1 else { return 0 }
        let p = 2 * n
        let m = ((i % p) + p) % p
        return m < n ? m : p - 1 - m
    }

    /// Area-weighted (box) resample of `channels`-byte pixels from `sw × sh` to `dw × dh`.
    /// Enlarging uses bilinear interpolation instead (a box would be blocky).
    static func resample(_ src: [UInt8], _ sw: Int, _ sh: Int, channels c: Int, to dw: Int, _ dh: Int) -> [UInt8] {
        if dw == sw, dh == sh { return src }
        if dw >= sw, dh >= sh { return bilinear(src, sw, sh, channels: c, to: dw, dh) }
        var out = [UInt8](repeating: 0, count: dw * dh * c)
        let sx = Double(sw) / Double(dw), sy = Double(sh) / Double(dh)
        for y in 0..<dh {
            let y0 = Double(y) * sy, y1 = y0 + sy
            for x in 0..<dw {
                let x0 = Double(x) * sx, x1 = x0 + sx
                var acc = SIMD4<Double>.zero // up to 4 channels, no per-pixel allocation
                var total = 0.0
                var yy = Int(y0)
                while Double(yy) < y1, yy < sh {
                    let wy = min(Double(yy + 1), y1) - max(Double(yy), y0)
                    var xx = Int(x0)
                    while Double(xx) < x1, xx < sw {
                        let wx = min(Double(xx + 1), x1) - max(Double(xx), x0)
                        let wt = wx * wy
                        if wt > 0 {
                            for k in 0..<min(c, 4) { acc[k] += Double(src[(yy * sw + xx) * c + k]) * wt }
                            total += wt
                        }
                        xx += 1
                    }
                    yy += 1
                }
                for k in 0..<min(c, 4) {
                    out[(y * dw + x) * c + k] = UInt8(min(255, max(0, (acc[k] / max(total, 1e-9)).rounded())))
                }
            }
        }
        return out
    }

    static func bilinear(_ src: [UInt8], _ sw: Int, _ sh: Int, channels c: Int, to dw: Int, _ dh: Int) -> [UInt8] {
        var out = [UInt8](repeating: 0, count: dw * dh * c)
        for y in 0..<dh {
            let fy = min(max((Double(y) + 0.5) * Double(sh) / Double(dh) - 0.5, 0), Double(sh - 1))
            let y0 = Int(fy), y1 = min(y0 + 1, sh - 1), ay = fy - Double(y0)
            for x in 0..<dw {
                let fx = min(max((Double(x) + 0.5) * Double(sw) / Double(dw) - 0.5, 0), Double(sw - 1))
                let x0 = Int(fx), x1 = min(x0 + 1, sw - 1), ax = fx - Double(x0)
                for k in 0..<c {
                    let a = Double(src[(y0 * sw + x0) * c + k]), b = Double(src[(y0 * sw + x1) * c + k])
                    let d = Double(src[(y1 * sw + x0) * c + k]), e = Double(src[(y1 * sw + x1) * c + k])
                    let v = (a * (1 - ax) + b * ax) * (1 - ay) + (d * (1 - ax) + e * ax) * ay
                    out[(y * dw + x) * c + k] = UInt8(min(255, max(0, v.rounded())))
                }
            }
        }
        return out
    }

    /// Mask resample where any covered source pixel makes the target pixel a hole (255).
    static func resampleMask(_ src: [UInt8], _ sw: Int, _ sh: Int, to dw: Int, _ dh: Int) -> [UInt8] {
        var out = [UInt8](repeating: 0, count: dw * dh)
        for y in 0..<dh {
            let y0 = Int(floor(Double(y) * Double(sh) / Double(dh)))
            let y1 = max(y0 + 1, Int(ceil(Double(y + 1) * Double(sh) / Double(dh))))
            for x in 0..<dw {
                let x0 = Int(floor(Double(x) * Double(sw) / Double(dw)))
                let x1 = max(x0 + 1, Int(ceil(Double(x + 1) * Double(sw) / Double(dw))))
                var hole = false
                for yy in y0..<min(y1, sh) where !hole {
                    for xx in x0..<min(x1, sw) where src[yy * sw + xx] > 0 {
                        hole = true
                        break
                    }
                }
                if hole { out[y * dw + x] = 255 }
            }
        }
        return out
    }

    /// The crop's RGB in the square (mirrored past the fitted extent).
    func squareRGB(_ rgb: [UInt8]) -> [UInt8] {
        let fw = fittedWidth, fh = fittedHeight
        let fitted = Self.resample(rgb, width, height, channels: 3, to: fw, fh)
        var out = [UInt8](repeating: 0, count: side * side * 3)
        for y in 0..<side {
            let sy = Self.mirror(y, fh)
            for x in 0..<side {
                let s = (sy * fw + Self.mirror(x, fw)) * 3, d = (y * side + x) * 3
                out[d] = fitted[s]
                out[d + 1] = fitted[s + 1]
                out[d + 2] = fitted[s + 2]
            }
        }
        return out
    }

    /// The crop's holes in the square (none in the mirrored margin).
    func squareMask(_ mask: [UInt8]) -> [UInt8] {
        let fw = fittedWidth, fh = fittedHeight
        let fitted = Self.resampleMask(mask, width, height, to: fw, fh)
        var out = [UInt8](repeating: 0, count: side * side)
        for y in 0..<fh { for x in 0..<fw { out[y * side + x] = fitted[y * fw + x] } }
        return out
    }

    /// The model's square output resized back to the crop, blended over the original by
    /// the mask (255 = filled; partial values blend; 0 = original, byte for byte).
    func composite(original: [UInt8], mask: [UInt8], filled: [UInt8]) -> [UInt8] {
        let fw = fittedWidth, fh = fittedHeight
        var region = [UInt8](repeating: 0, count: fw * fh * 3)
        for y in 0..<fh {
            for x in 0..<fw {
                let s = (y * side + x) * 3, d = (y * fw + x) * 3
                region[d] = filled[s]
                region[d + 1] = filled[s + 1]
                region[d + 2] = filled[s + 2]
            }
        }
        let back = Self.resample(region, fw, fh, channels: 3, to: width, height)
        var out = original
        for i in 0..<(width * height) where mask[i] > 0 {
            let a = Int(mask[i])
            for k in 0..<3 {
                let o = Int(original[i * 3 + k]), f = Int(back[i * 3 + k])
                out[i * 3 + k] = UInt8((f * a + o * (255 - a) + 127) / 255)
            }
        }
        return out
    }
}
