// Generates the app icon from the viewfinder identity in
// docs/brand/darkslide-logo.png: a dark body, focus brackets, a heavy "d"
// drawn as paths (no font dependency, so the output is reproducible), a dashed
// level line and an exposure tick scale. Detail is dropped at small sizes.
// Usage: swift scripts/make-icon.swift [app] <out.iconset>
// then `iconutil -c icns <out.iconset>`. The results are committed under
// app/Resources, so this only runs when the design changes.
import AppKit

// `make-icon.swift <out.iconset>` (the original form) and `make-icon.swift app <out.iconset>` both draw the app icon.
let cli = Array(CommandLine.arguments.dropFirst())
guard (cli.count == 1) || (cli.count == 2 && cli[0] == "app") else {
    FileHandle.standardError.write(Data("usage: swift scripts/make-icon.swift [app] <out.iconset>\n".utf8))
    exit(2)
}
let out = URL(fileURLWithPath: cli[cli.count - 1])
try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)

let cs = CGColorSpace(name: CGColorSpace.displayP3)!
func color(_ w: CGFloat, _ a: CGFloat = 1) -> CGColor { CGColor(colorSpace: cs, components: [w, w, w * 0.98, a])! }
let ink = color(0.96)

/// Everything below is laid out on a 1024 grid and scaled by the context.
func corner(_ ctx: CGContext, _ x: CGFloat, _ y: CGFloat, _ dx: CGFloat, _ dy: CGFloat, len: CGFloat, w: CGFloat) {
    ctx.setStrokeColor(color(0.78))
    ctx.setLineWidth(w)
    ctx.setLineCap(.butt)
    ctx.move(to: CGPoint(x: x + dx * len, y: y))
    ctx.addLine(to: CGPoint(x: x, y: y))
    ctx.addLine(to: CGPoint(x: x, y: y + dy * len))
    ctx.strokePath()
}

/// The bold lowercase "d": a ring plus a stem, like the wordmark's.
func glyph(_ ctx: CGContext, cx: CGFloat, cy: CGFloat, r: CGFloat, weight: CGFloat, stemTop: CGFloat) {
    ctx.setStrokeColor(ink)
    ctx.setLineWidth(weight)
    ctx.addEllipse(in: CGRect(x: cx - (r - weight / 2), y: cy - (r - weight / 2), width: 2 * r - weight, height: 2 * r - weight))
    ctx.strokePath()
    ctx.setFillColor(ink)
    ctx.fill(CGRect(x: cx + r - weight, y: cy - r, width: weight, height: stemTop - (cy - r)))
}

/// Dashed level line, tilted a few degrees, with arrow ends; a dark knockout
/// under it keeps it visible where it crosses the glyph.
func level(_ ctx: CGContext, cx: CGFloat, cy: CGFloat, half: CGFloat, w: CGFloat) {
    let tilt: CGFloat = -0.05
    ctx.saveGState()
    ctx.translateBy(x: cx, y: cy)
    ctx.rotate(by: tilt)
    let head = w * 2.6
    ctx.setStrokeColor(color(0.04))
    ctx.setLineWidth(w * 2.8)
    ctx.move(to: CGPoint(x: -half + head, y: 0))
    ctx.addLine(to: CGPoint(x: half - head, y: 0))
    ctx.strokePath()
    ctx.setStrokeColor(ink)
    ctx.setLineWidth(w)
    ctx.setLineDash(phase: 0, lengths: [w * 5, w * 3.2])
    ctx.move(to: CGPoint(x: -half + head, y: 0))
    ctx.addLine(to: CGPoint(x: half - head, y: 0))
    ctx.strokePath()
    ctx.setLineDash(phase: 0, lengths: [])
    ctx.setFillColor(ink)
    ctx.move(to: CGPoint(x: -half, y: 0))
    ctx.addLine(to: CGPoint(x: -half + head, y: head * 0.6))
    ctx.addLine(to: CGPoint(x: -half + head, y: -head * 0.6))
    ctx.fillPath()
    ctx.move(to: CGPoint(x: half, y: 0))
    ctx.addLine(to: CGPoint(x: half - head, y: head * 0.6))
    ctx.addLine(to: CGPoint(x: half - head, y: -head * 0.6))
    ctx.fillPath()
    ctx.restoreGState()
}

/// Exposure scale: 13 ticks, a taller bold one at 0.
func ticks(_ ctx: CGContext, cx: CGFloat, y: CGFloat, width: CGFloat) {
    let n = 13
    for i in 0..<n {
        let x = cx - width / 2 + width * CGFloat(i) / CGFloat(n - 1)
        let mid = i == n / 2
        ctx.setStrokeColor(mid ? ink : color(0.6))
        ctx.setLineWidth(mid ? 12 : 7)
        ctx.move(to: CGPoint(x: x, y: y))
        ctx.addLine(to: CGPoint(x: x, y: y + (mid ? 46 : (i % 2 == 0 ? 30 : 20))))
        ctx.strokePath()
    }
}

func newContext(_ px: Int) -> CGContext {
    let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    ctx.scaleBy(x: CGFloat(px) / 1024, y: CGFloat(px) / 1024)
    return ctx
}

func darkFill(_ ctx: CGContext, _ rect: CGRect) {
    let g = CGGradient(colorsSpace: cs, colors: [color(0.13), color(0.035)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(g, start: CGPoint(x: 0, y: rect.maxY), end: CGPoint(x: 0, y: rect.minY), options: [])
}

func appIcon(_ px: Int) -> CGContext {
    let ctx = newContext(px)
    let tiny = px <= 16, small = px <= 32
    // macOS icon grid: 824/1024 body with rounded corners.
    let body = CGRect(x: 100, y: 100, width: 824, height: 824)
    ctx.saveGState()
    ctx.addPath(CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil))
    ctx.clip()
    darkFill(ctx, body)
    if tiny {
        glyph(ctx, cx: 450, cy: 470, r: 270, weight: 160, stemTop: 780)
    } else {
        corner(ctx, 220, 804, 1, -1, len: small ? 150 : 120, w: small ? 44 : 22)
        corner(ctx, 804, 804, -1, -1, len: small ? 150 : 120, w: small ? 44 : 22)
        corner(ctx, 220, 220, 1, 1, len: small ? 150 : 120, w: small ? 44 : 22)
        corner(ctx, 804, 220, -1, 1, len: small ? 150 : 120, w: small ? 44 : 22)
        let cy: CGFloat = small ? 500 : 530
        let r: CGFloat = small ? 215 : 190
        glyph(ctx, cx: 462, cy: cy, r: r, weight: small ? 120 : 98, stemTop: cy + r + 90)
        if !small {
            level(ctx, cx: 512, cy: 530, half: 330, w: 11)
            ticks(ctx, cx: 512, y: 250, width: 300)
        }
    }
    ctx.restoreGState()
    return ctx
}

func png(_ ctx: CGContext) -> Data {
    NSBitmapImageRep(cgImage: ctx.makeImage()!).representation(using: .png, properties: [:])!
}

for (name, px) in [("16x16", 16), ("16x16@2x", 32), ("32x32", 32), ("32x32@2x", 64), ("128x128", 128),
                   ("128x128@2x", 256), ("256x256", 256), ("256x256@2x", 512), ("512x512", 512), ("512x512@2x", 1024)] {
    try png(appIcon(px)).write(to: out.appendingPathComponent("icon_\(name).png"))
}
