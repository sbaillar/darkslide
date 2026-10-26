// Draws the Luxtable identity: the app icon and the wordmark, in the viewfinder
// style (a dark body, focus brackets, a dashed level line, an exposure scale).
// The icon is a lit light table (lux + table) with a bold "lt" on it; the wordmark
// is "luxtable". Every letter is drawn as paths (rings, stems, bars), so there is
// no font dependency and the output is reproducible.
//
// Usage:
//   swift scripts/make-icon.swift [app] <out.iconset>   then `iconutil -c icns <out.iconset>`
//   swift scripts/make-icon.swift wordmark <out.png>    the 1774x887 wordmark (docs/brand, site)
//   swift scripts/make-icon.swift png <size> <out.png>  the icon alone at one size (favicons)
// The results are committed (app/Resources, docs/brand, site), so this only runs when
// the design changes.
import AppKit

let cli = Array(CommandLine.arguments.dropFirst())
let cs = CGColorSpace(name: CGColorSpace.displayP3)!
func color(_ w: CGFloat, _ a: CGFloat = 1) -> CGColor { CGColor(colorSpace: cs, components: [w, w, w * 0.98, a])! }
func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: cs, components: [r, g, b, a])!
}
let ink = color(0.96)
/// The light table's warm white, and the dark letters on it.
let lampHot = rgb(1.0, 0.985, 0.94), lampEdge = rgb(0.93, 0.89, 0.80), glow = rgb(1.0, 0.93, 0.78)
let onLamp = color(0.07)

// MARK: - Letters

/// Geometric lowercase letters on a baseline, all in one stroke weight `w`, x-height
/// `h` (bowls are circles of diameter `h`) and ascender `asc`. Each returns its advance.
struct Letters {
    var ctx: CGContext
    var w: CGFloat
    var h: CGFloat
    var asc: CGFloat
    var fill: CGColor
    var r: CGFloat { h / 2 }

    func ring(_ cx: CGFloat, _ cy: CGFloat, from a0: CGFloat = 0, to a1: CGFloat = 2 * .pi) {
        ctx.setStrokeColor(fill)
        ctx.setLineWidth(w)
        ctx.setLineCap(.butt)
        ctx.addArc(center: CGPoint(x: cx, y: cy), radius: r - w / 2, startAngle: a0, endAngle: a1, clockwise: false)
        ctx.strokePath()
    }

    func bar(_ x: CGFloat, _ y: CGFloat, _ width: CGFloat, _ height: CGFloat) {
        ctx.setFillColor(fill)
        ctx.fill(CGRect(x: x, y: y, width: width, height: height))
    }

    func l(_ x: CGFloat, _ y: CGFloat) -> CGFloat {
        bar(x, y, w, asc)
        return w
    }

    func u(_ x: CGFloat, _ y: CGFloat) -> CGFloat {
        ctx.setStrokeColor(fill)
        ctx.setLineWidth(w)
        ctx.setLineCap(.butt)
        ctx.move(to: CGPoint(x: x + w / 2, y: y + h))
        ctx.addLine(to: CGPoint(x: x + w / 2, y: y + r))
        ctx.addArc(center: CGPoint(x: x + r, y: y + r), radius: r - w / 2, startAngle: .pi, endAngle: 2 * .pi, clockwise: false)
        ctx.strokePath()
        bar(x + 2 * r - w, y, w, h)
        return 2 * r
    }

    func x(_ x: CGFloat, _ y: CGFloat) -> CGFloat {
        let width = 2 * r * 0.92
        ctx.saveGState()
        ctx.clip(to: CGRect(x: x - w, y: y, width: width + 2 * w, height: h))
        ctx.setStrokeColor(fill)
        ctx.setLineWidth(w * 1.04)
        ctx.setLineCap(.butt)
        let over = w * 0.6
        ctx.move(to: CGPoint(x: x + w * 0.42 - over * 0.5, y: y - over))
        ctx.addLine(to: CGPoint(x: x + width - w * 0.42 + over * 0.5, y: y + h + over))
        ctx.move(to: CGPoint(x: x + w * 0.42 - over * 0.5, y: y + h + over))
        ctx.addLine(to: CGPoint(x: x + width - w * 0.42 + over * 0.5, y: y - over))
        ctx.strokePath()
        ctx.restoreGState()
        return width
    }

    /// A stem with a crossbar at the x-height and a foot turning right (without the
    /// foot a bold "t" reads as a plus at icon sizes).
    func t(_ x: CGFloat, _ y: CGFloat) -> CGFloat {
        let cx = x + w * 1.1 + w / 2 // stem centre line
        let hook = w * 0.95
        ctx.setStrokeColor(fill)
        ctx.setLineWidth(w)
        ctx.setLineCap(.butt)
        ctx.move(to: CGPoint(x: cx, y: y + asc * 0.86))
        ctx.addLine(to: CGPoint(x: cx, y: y + w / 2 + hook))
        ctx.addArc(center: CGPoint(x: cx + hook, y: y + w / 2 + hook), radius: hook,
                   startAngle: .pi, endAngle: 1.5 * .pi, clockwise: false)
        ctx.addLine(to: CGPoint(x: cx + hook + w * 0.55, y: y + w / 2))
        ctx.strokePath()
        let right = cx + hook + w * 0.55
        bar(x, y + h - w, right - x, w)
        return right - x
    }

    func a(_ x: CGFloat, _ y: CGFloat) -> CGFloat {
        ring(x + r, y + r)
        bar(x + 2 * r - w, y, w, h)
        return 2 * r
    }

    func b(_ x: CGFloat, _ y: CGFloat) -> CGFloat {
        ring(x + r, y + r)
        bar(x, y, w, asc)
        return 2 * r
    }

    func e(_ x: CGFloat, _ y: CGFloat) -> CGFloat {
        // From the bar's right end round over the top and the bottom, open at the lower right.
        ring(x + r, y + r, from: 0, to: 2 * .pi * 0.88)
        bar(x + w / 2, y + r - w / 2, 2 * r - w / 2, w)
        return 2 * r
    }

    /// Draw `word` from `x` (left) on baseline `y`; returns the width drawn.
    @discardableResult
    func word(_ word: String, x: CGFloat, y: CGFloat, gap: CGFloat) -> CGFloat {
        var at = x
        for (i, ch) in word.enumerated() {
            if i > 0 { at += gap }
            switch ch {
            case "l": at += l(at, y)
            case "u": at += u(at, y)
            case "x": at += self.x(at, y)
            case "t": at += t(at, y)
            case "a": at += a(at, y)
            case "b": at += b(at, y)
            case "e": at += e(at, y)
            default: fatalError("no glyph for \(ch)")
            }
        }
        return at - x
    }

    /// The width `word` would take (drawing into a throwaway context).
    func measure(_ word: String, gap: CGFloat) -> CGFloat {
        let scratch = CGContext(data: nil, width: 4, height: 4, bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        return Letters(ctx: scratch, w: w, h: h, asc: asc, fill: fill).word(word, x: 0, y: 0, gap: gap)
    }
}

// MARK: - Viewfinder chrome

func corner(_ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ dx: CGFloat, _ dy: CGFloat, len: CGFloat, w: CGFloat,
            shade: CGFloat = 0.78) {
    ctx.setStrokeColor(color(shade))
    ctx.setLineWidth(w)
    ctx.setLineCap(.butt)
    ctx.move(to: CGPoint(x: x + dx * len, y: y))
    ctx.addLine(to: CGPoint(x: x, y: y))
    ctx.addLine(to: CGPoint(x: x, y: y + dy * len))
    ctx.strokePath()
}

/// Dashed level line, tilted a few degrees, with arrow ends.
func level(_ ctx: CGContext, cx: CGFloat, cy: CGFloat, half: CGFloat, w: CGFloat) {
    ctx.saveGState()
    ctx.translateBy(x: cx, y: cy)
    ctx.rotate(by: -0.035)
    let head = w * 2.6
    ctx.setStrokeColor(ink)
    ctx.setLineWidth(w)
    ctx.setLineDash(phase: 0, lengths: [w * 6, w * 4])
    ctx.move(to: CGPoint(x: -half + head, y: 0))
    ctx.addLine(to: CGPoint(x: half - head, y: 0))
    ctx.strokePath()
    ctx.setLineDash(phase: 0, lengths: [])
    ctx.setFillColor(ink)
    for s: CGFloat in [-1, 1] {
        ctx.move(to: CGPoint(x: s * half, y: 0))
        ctx.addLine(to: CGPoint(x: s * (half - head), y: head * 0.6))
        ctx.addLine(to: CGPoint(x: s * (half - head), y: -head * 0.6))
        ctx.fillPath()
    }
    ctx.restoreGState()
}

/// Exposure scale: 13 ticks, a taller bold one at 0.
func ticks(_ ctx: CGContext, cx: CGFloat, y: CGFloat, width: CGFloat, scale s: CGFloat = 1) {
    let n = 13
    for i in 0..<n {
        let x = cx - width / 2 + width * CGFloat(i) / CGFloat(n - 1)
        let mid = i == n / 2
        ctx.setStrokeColor(mid ? ink : color(0.6))
        ctx.setLineWidth((mid ? 12 : 7) * s)
        ctx.move(to: CGPoint(x: x, y: y))
        ctx.addLine(to: CGPoint(x: x, y: y + (mid ? 46 : (i % 2 == 0 ? 30 : 20)) * s))
        ctx.strokePath()
    }
}

// MARK: - Icon

func newContext(_ wPx: Int, _ hPx: Int, unit: CGFloat) -> CGContext {
    let ctx = CGContext(data: nil, width: wPx, height: hPx, bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    ctx.scaleBy(x: CGFloat(wPx) / unit, y: CGFloat(wPx) / unit)
    return ctx
}

func darkFill(_ ctx: CGContext, _ rect: CGRect) {
    let g = CGGradient(colorsSpace: cs, colors: [color(0.13), color(0.035)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(g, start: CGPoint(x: 0, y: rect.maxY), end: CGPoint(x: 0, y: rect.minY), options: [])
}

/// The light table: a warm, evenly lit panel with a soft glow around it.
func lamp(_ ctx: CGContext, _ panel: CGRect, radius: CGFloat, glowBlur: CGFloat) {
    let path = CGPath(roundedRect: panel, cornerWidth: radius, cornerHeight: radius, transform: nil)
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: glowBlur, color: glow.copy(alpha: 0.55))
    ctx.addPath(path)
    ctx.setFillColor(lampEdge)
    ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addPath(path)
    ctx.clip()
    let g = CGGradient(colorsSpace: cs, colors: [lampHot, lampEdge] as CFArray, locations: [0, 1])!
    let c = CGPoint(x: panel.midX, y: panel.midY + panel.height * 0.08)
    ctx.drawRadialGradient(g, startCenter: c, startRadius: 0, endCenter: c, endRadius: panel.width * 0.78,
                           options: [.drawsAfterEndLocation])
    ctx.restoreGState()
}

func appIcon(_ px: Int) -> CGContext {
    let ctx = newContext(px, px, unit: 1024)
    let tiny = px <= 16, small = px <= 32
    // macOS icon grid: 824/1024 body with rounded corners.
    let body = CGRect(x: 100, y: 100, width: 824, height: 824)
    ctx.saveGState()
    ctx.addPath(CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil))
    ctx.clip()
    darkFill(ctx, body)
    // Small sizes: a bigger panel and heavier letters, no brackets.
    let panel = tiny || small ? CGRect(x: 200, y: 200, width: 624, height: 624) : CGRect(x: 282, y: 300, width: 460, height: 460)
    lamp(ctx, panel, radius: small ? 70 : 46, glowBlur: small ? 30 : 70)
    let weight: CGFloat = tiny ? 120 : (small ? 100 : 66)
    let xh: CGFloat = tiny ? 300 : (small ? 270 : 200)
    let letters = Letters(ctx: ctx, w: weight, h: xh, asc: xh * 1.55, fill: onLamp)
    let gap = weight * 0.45
    let width = letters.measure("lt", gap: gap)
    let baseline = panel.midY - letters.asc * 0.5
    letters.word("lt", x: panel.midX - width / 2, y: baseline, gap: gap)
    if !small {
        let len: CGFloat = 112, w: CGFloat = 22
        corner(ctx, 200, 824, 1, -1, len: len, w: w)
        corner(ctx, 824, 824, -1, -1, len: len, w: w)
        corner(ctx, 200, 200, 1, 1, len: len, w: w)
        corner(ctx, 824, 200, -1, 1, len: len, w: w)
        ticks(ctx, cx: 512, y: 214, width: 260, scale: 0.8)
    }
    ctx.restoreGState()
    return ctx
}

// MARK: - Wordmark

/// "luxtable" in the viewfinder, on the brand's near-black, 1774x887.
func wordmark() -> CGContext {
    let (wPx, hPx) = (1774, 887)
    let ctx = newContext(wPx, hPx, unit: CGFloat(wPx))
    ctx.setFillColor(color(0.045))
    ctx.fill(CGRect(x: 0, y: 0, width: wPx, height: hPx))
    let (W, H) = (CGFloat(wPx), CGFloat(hPx))
    // Focus brackets.
    let len: CGFloat = 64, bw: CGFloat = 4
    corner(ctx, 224, H - 198, 1, -1, len: len, w: bw, shade: 0.85)
    corner(ctx, W - 224, H - 198, -1, -1, len: len, w: bw, shade: 0.85)
    corner(ctx, 224, 230, 1, 1, len: len, w: bw, shade: 0.85)
    corner(ctx, W - 224, 230, -1, 1, len: len, w: bw, shade: 0.85)
    // A warm light-table glow behind the word.
    ctx.saveGState()
    let g = CGGradient(colorsSpace: cs, colors: [glow.copy(alpha: 0.22)!, glow.copy(alpha: 0)!] as CFArray, locations: [0, 1])!
    let c = CGPoint(x: W / 2, y: H / 2)
    ctx.drawRadialGradient(g, startCenter: c, startRadius: 0, endCenter: c, endRadius: 560, options: [])
    ctx.restoreGState()
    let letters = Letters(ctx: ctx, w: 40, h: 150, asc: 222, fill: ink)
    let gap: CGFloat = 22
    let width = letters.measure("luxtable", gap: gap)
    let baseline = H / 2 - 222 / 2 + 6
    level(ctx, cx: W / 2, cy: H / 2 + 34, half: 660, w: 4)
    letters.word("luxtable", x: (W - width) / 2, y: baseline, gap: gap)
    ticks(ctx, cx: W / 2, y: 92, width: 360, scale: 0.6)
    return ctx
}

func png(_ ctx: CGContext) -> Data {
    NSBitmapImageRep(cgImage: ctx.makeImage()!).representation(using: .png, properties: [:])!
}

func usage() -> Never {
    FileHandle.standardError.write(Data("""
    usage: swift scripts/make-icon.swift [app] <out.iconset>
           swift scripts/make-icon.swift wordmark <out.png>
           swift scripts/make-icon.swift png <size> <out.png>

    """.utf8))
    exit(2)
}

switch cli.first {
case "wordmark" where cli.count == 2:
    try png(wordmark()).write(to: URL(fileURLWithPath: cli[1]))
case "png" where cli.count == 3:
    guard let px = Int(cli[1]), px > 0 else { usage() }
    try png(appIcon(px)).write(to: URL(fileURLWithPath: cli[2]))
case _ where cli.count == 1 || (cli.count == 2 && cli[0] == "app"):
    let out = URL(fileURLWithPath: cli[cli.count - 1])
    try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
    for (name, px) in [("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64), ("128x128", 128),
                       ("128x128@2x", 256), ("256x256", 256), ("256x256@2x", 512), ("512x512", 512), ("512x512@2x", 1024)] {
        try png(appIcon(px)).write(to: out.appendingPathComponent("icon_\(name).png"))
    }
default:
    usage()
}
