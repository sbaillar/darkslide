import LuxtableCore
import SwiftUI

/// Crop rectangle with handles, straighten line, gradient/brush in progress, and
/// the selected layer's mask shapes (vector and Vision bitmaps), drawn over the
/// image in display space. Masks are stored in oriented space and converted
/// through `ViewerModel.displayed(_:)`.
struct CanvasOverlay: View {
    let viewer: ViewerModel

    var body: some View {
        Canvas { ctx, _ in
            guard let pane = viewer.layout.main else { return }
            func pt(_ u: CGPoint) -> CGPoint { pane.viewPoint(u) }
            func viewRect(_ r: CGRect) -> CGRect {
                CGRect(origin: pt(r.origin), size: CGSize(width: r.width * pane.image.width, height: r.height * pane.image.height))
            }
            let accent = DS.accent

            // Vision masks and brush strokes are drawn by the Metal pass (GPU); only
            // vector handles (gradients, crop, straighten) are drawn here.
            if viewer.selectedLayerID != nil {
                if let mask = viewer.selectedMask {
                    for shape in mask.shapes { draw(shape, pane: pane, in: &ctx, tint: .red.opacity(0.35)) }
                }
            }
            let crop = viewer.cropDragPreview ?? viewer.cropRect
            if viewer.activeTool == .crop || crop != CGRect(x: 0, y: 0, width: 1, height: 1) {
                let rect = viewRect(crop)
                if viewer.activeTool == .crop {
                    // Outside the image area (lens correction's curved edges, straighten's
                    // corners: what no export shows) is hidden; the rest outside the crop dimmed.
                    let area = viewer.cropImageArea.map(viewRect)
                    if let area {
                        var hidden = Path(pane.image.insetBy(dx: -1, dy: -1))
                        hidden.addRect(area)
                        ctx.fill(hidden, with: .color(DS.viewerBackground), style: FillStyle(eoFill: true))
                    }
                    var outside = Path(area ?? pane.image)
                    outside.addRect(rect)
                    ctx.fill(outside, with: .color(.black.opacity(0.65)), style: FillStyle(eoFill: true)) // C1/LR-strength dim
                    if let area, area != rect {
                        ctx.stroke(Path(area), with: .color(.white.opacity(0.3)), style: StrokeStyle(lineWidth: 1, dash: [4, 3]))
                    }
                    for i in 1...2 {
                        let x = rect.minX + rect.width * CGFloat(i) / 3, y = rect.minY + rect.height * CGFloat(i) / 3
                        ctx.stroke(Path { $0.move(to: CGPoint(x: x, y: rect.minY)); $0.addLine(to: CGPoint(x: x, y: rect.maxY)) },
                                   with: .color(.white.opacity(0.35)), lineWidth: 0.5)
                        ctx.stroke(Path { $0.move(to: CGPoint(x: rect.minX, y: y)); $0.addLine(to: CGPoint(x: rect.maxX, y: y)) },
                                   with: .color(.white.opacity(0.35)), lineWidth: 0.5)
                    }
                    // Handles: corners and edge midpoints (drag to resize; drag inside to move).
                    let handles = [CGPoint(x: rect.minX, y: rect.minY), CGPoint(x: rect.maxX, y: rect.minY),
                                   CGPoint(x: rect.maxX, y: rect.maxY), CGPoint(x: rect.minX, y: rect.maxY),
                                   CGPoint(x: rect.midX, y: rect.minY), CGPoint(x: rect.maxX, y: rect.midY),
                                   CGPoint(x: rect.midX, y: rect.maxY), CGPoint(x: rect.minX, y: rect.midY)]
                    for h in handles {
                        ctx.fill(Path(CGRect(x: h.x - 4, y: h.y - 4, width: 8, height: 8)), with: .color(.white))
                    }
                }
                ctx.stroke(Path(rect), with: .color(viewer.cropDragPreview != nil ? accent : .white.opacity(0.9)),
                           lineWidth: viewer.cropDragPreview != nil ? 1.5 : 1)
            }
            guard let d = viewer.activeDrag else { return }
            switch d.tool {
            case .straighten, .linearGradient:
                ctx.stroke(Path { $0.move(to: pt(d.start)); $0.addLine(to: pt(d.current)) }, with: .color(accent), lineWidth: 1.5)
                for p in [d.start, d.current] {
                    ctx.fill(Path(ellipseIn: CGRect(x: pt(p).x - 4, y: pt(p).y - 4, width: 8, height: 8)), with: .color(accent))
                }
            case .radialGradient:
                let c = pt(d.start), e = pt(d.current)
                let r = max(abs(e.x - c.x), abs(e.y - c.y))
                ctx.stroke(Path(ellipseIn: CGRect(x: c.x - r, y: c.y - r, width: 2 * r, height: 2 * r)),
                           with: .color(accent), lineWidth: 1.5)
            default: break // brush/eraser previews are drawn in the Metal pass
            }
        }
        .allowsHitTesting(false)
    }

    private func draw(_ oriented: MaskShape, pane: ViewerLayout.Pane, in ctx: inout GraphicsContext, tint: Color) {
        func pt(_ u: CGPoint) -> CGPoint { pane.viewPoint(u) }
        let aspect = pane.image.width / max(pane.image.height, 1e-6)
        let shape = viewer.transform.toDisplay(oriented, displayAspect: aspect)
        switch shape {
        case .fill: ctx.fill(Path(pane.image), with: .color(tint))
        case .brush: break // Metal pass
        case .linear(_, let a, let b):
            ctx.stroke(Path { $0.move(to: pt(a)); $0.addLine(to: pt(b)) }, with: .color(.white), lineWidth: 1)
        case .radial(_, let c, let r, let angle, _):
            ctx.stroke(ViewerOverlayGeometry.radialPath(center: pt(c), radius: r, angleDegrees: angle, imageSize: pane.image.size),
                       with: .color(.white), lineWidth: 1)
        case .ai, .aiPerson: break // drawn from the Vision bitmap
        }
        // [lay9] Editable gradients: drag a handle with the gradient tool to change it.
        for (_, h) in MaskHandles.handles(of: shape, aspect: aspect) {
            let c = pt(h)
            let dot = Path(ellipseIn: CGRect(x: c.x - 4.5, y: c.y - 4.5, width: 9, height: 9))
            ctx.fill(dot, with: .color(.white))
            ctx.stroke(dot, with: .color(DS.accent), lineWidth: 1.5)
        }
    }
}

/// Mini image with the visible rect; drag to pan. Shown when zoomed in.
struct NavigatorOverlay: View {
    let viewer: ViewerModel

    var body: some View {
        if viewer.showNavigator, viewer.state.zoom != .fit, let img = viewer.navigatorImage, let pane = viewer.layout.main {
            let aspect = CGFloat(img.width) / CGFloat(max(img.height, 1))
            let size = aspect >= 1 ? CGSize(width: 180, height: 180 / aspect) : CGSize(width: 180 * aspect, height: 180)
            let v = pane.visibleRegion
            Image(decorative: img, scale: 1)
                .resizable()
                .frame(width: size.width, height: size.height)
                .overlay(alignment: .topLeading) {
                    Rectangle()
                        .strokeBorder(DS.accent, lineWidth: 1.5)
                        .frame(width: v.width * size.width, height: v.height * size.height)
                        .offset(x: v.minX * size.width, y: v.minY * size.height)
                }
                .clipShape(.rect(cornerRadius: DS.Radius.s))
                .gesture(DragGesture(minimumDistance: 0).onChanged { g in
                    viewer.center(on: CGPoint(x: g.location.x / size.width, y: g.location.y / size.height))
                })
                .padding(DS.Space.xs)
                .glassEffect(.regular, in: .rect(cornerRadius: DS.Radius.m))
                .help("Navigator: drag to move around the image")
                .accessibilityLabel("Navigator")
        }
    }
}
