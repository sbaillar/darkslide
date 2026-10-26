import CoreGraphics
import LuxtableFFI
import Foundation

/// A copied crop landing on another photo (M10): lt-core `clipboard::adapt_crop`
/// through lt-ffi, so the app follows the one Rust rule. The orientation follows the
/// frame (or is kept for a deliberate portrait-of-landscape crop), and a locked aspect
/// is re-fitted in the target's pixels: a 4:5 crop pastes as 4:5 pixels.
enum CropPaste {
    /// `crop` copied from a photo of `source` oriented pixels, for one of `target`.
    /// Only the rect changes; unknown sizes paste it as is.
    static func adapt(_ crop: CropEdit, from source: CGSize, to target: CGSize) -> CropEdit {
        func px(_ v: CGFloat) -> UInt32? { v.isFinite && v >= 1 && v <= CGFloat(UInt32.max) ? UInt32(v.rounded()) : nil }
        guard let sw = px(source.width), let sh = px(source.height), let tw = px(target.width), let th = px(target.height)
        else { return crop }
        let ratio = crop.aspect.ratio
        let r = crop.rect
        let input = PastedCrop(x: Float(r.minX), y: Float(r.minY), w: Float(r.width), h: Float(r.height),
                               aspectLong: UInt32(ratio?.long ?? 0), aspectShort: UInt32(ratio?.short ?? 0),
                               quarterTurns: UInt8(((crop.quarterTurns % 4) + 4) % 4))
        let out = adaptPastedCrop(crop: input, sourceWidth: sw, sourceHeight: sh, targetWidth: tw, targetHeight: th)
        // Values the rule left as they were keep their exact Double (no Float round-trip noise).
        func exact(_ o: Float, _ v: CGFloat) -> CGFloat { abs(Double(o) - Double(v)) < 1e-6 ? v : CGFloat(o) }
        var c = crop
        c.rect = CGRect(x: exact(out.x, r.minX), y: exact(out.y, r.minY), width: exact(out.w, r.width), height: exact(out.h, r.height))
        return c
    }

    /// `adapt` for a target shown with `targetTurns` quarter turns (the source is shown with
    /// the crop's own): the rule runs on the displayed frames, so a portrait crop stays
    /// portrait on a landscape photo the user turned upright. The result has `targetTurns`.
    static func adapt(_ crop: CropEdit, from source: CGSize, to target: CGSize, targetTurns: Int) -> CropEdit {
        let s = ((crop.quarterTurns % 4) + 4) % 4, t = ((targetTurns % 4) + 4) % 4
        guard s != t else { return adapt(crop, from: source, to: target) }
        func turned(_ z: CGSize, _ n: Int) -> CGSize { n % 2 == 1 ? CGSize(width: z.height, height: z.width) : z }
        var shown = crop
        shown.quarterTurns = 0
        shown.rect = turn(crop.rect, by: s)
        var out = adapt(shown, from: turned(source, s), to: turned(target, t))
        out.rect = turn(out.rect, by: 4 - t)
        out.quarterTurns = t
        return out
    }

    /// A normalized rect turned `n` quarter turns clockwise with its frame (lt-pipeline's turn:
    /// (u, v) → (1 − v, u)).
    static func turn(_ r: CGRect, by n: Int) -> CGRect {
        var r = r
        for _ in 0..<(((n % 4) + 4) % 4) {
            r = CGRect(x: 1 - r.minY - r.height, y: r.minX, width: r.height, height: r.width)
        }
        return r
    }

    /// Shrink a straightened crop about its centre, keeping its aspect, until its corners lie
    /// inside the frame of `size` oriented pixels. lt-pipeline auto-fits only a full-frame
    /// rect (`plan_crop`); any other rect is used as is, in the viewer and in export, so a
    /// rect that fits at one angle shows empty wedges at another (a straighten kept on paste).
    /// Same corner rule as the pipeline's `fits` (half a pixel of slack), without lens or keystone.
    static func inscribe(_ crop: CropEdit, in size: CGSize) -> CropEdit {
        let r = crop.rect
        guard crop.angle != 0, size.width >= 1, size.height >= 1, r != CGRect(x: 0, y: 0, width: 1, height: 1) else { return crop }
        guard !fits(r, angle: crop.angle, in: size) else { return crop }
        var (lo, hi) = (0.0, 1.0)
        for _ in 0..<40 {
            let m = (lo + hi) / 2
            if fits(scaled(r, m), angle: crop.angle, in: size) { lo = m } else { hi = m }
        }
        var c = crop
        c.rect = scaled(r, lo)
        return c
    }

    /// Whether every corner of `rect`, turned by `angle` degrees about its centre (lt-pipeline:
    /// crop-local → E), lies in the frame.
    static func fits(_ rect: CGRect, angle: Double, in size: CGSize) -> Bool {
        let (w, h) = (Double(size.width), Double(size.height))
        let a = angle * .pi / 180
        let (sn, cs) = (sin(a), cos(a))
        let (cx, cy) = (Double(rect.midX) * w, Double(rect.midY) * h)
        let (hx, hy) = (Double(rect.width) * w / 2, Double(rect.height) * h / 2)
        return [(-1.0, -1.0), (1, -1), (-1, 1), (1, 1)].allSatisfy { sx, sy in
            let (lx, ly) = (sx * hx, sy * hy)
            let (x, y) = (cx + cs * lx - sn * ly, cy + sn * lx + cs * ly)
            return x >= -0.5 && y >= -0.5 && x <= w + 0.5 && y <= h + 0.5
        }
    }

    private static func scaled(_ r: CGRect, _ k: Double) -> CGRect {
        let k = CGFloat(k)
        return CGRect(x: r.midX - r.width * k / 2, y: r.midY - r.height * k / 2, width: r.width * k, height: r.height * k)
    }
}
