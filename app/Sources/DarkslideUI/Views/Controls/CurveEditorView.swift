import DarkslideCore
import SwiftUI

/// Tone curve editor: click to add, drag to move, drag out of the box or
/// double-click to delete. The line is `CurveModel`'s monotone cubic.
struct CurveEditorView: View {
    /// Stored points (`[]` = identity).
    let points: [CurvePoint]
    var color: Color = .primary
    var histogram: [Double]?
    let onChange: ([CurvePoint]) -> Void

    @State private var dragIndex: Int?
    /// Points while dragging (so a point dragged out can come back before release).
    @State private var working: CurveModel?

    private static let hitRadius = 0.05

    var body: some View {
        content
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(ControlDescriptions.curve(points).label)
            .accessibilityValue(ControlDescriptions.curve(points).value)
            // M11: brighten / darken the midtones with the adjust gesture; reset as an action.
            .accessibilityAdjustableAction { dir in
                onChange(ControlAdjust.curveMidtones(points, steps: dir == .increment ? 1 : -1))
            }
            .accessibilityAction(named: "Reset curve") { onChange([]) }
    }

    private var content: some View {
        GeometryReader { geo in
            let size = geo.size
            let model = working ?? CurveModel(stored: points)
            Canvas { ctx, sz in
                for i in 1..<4 {
                    let f = CGFloat(i) / 4
                    var grid = Path()
                    grid.move(to: CGPoint(x: sz.width * f, y: 0)); grid.addLine(to: CGPoint(x: sz.width * f, y: sz.height))
                    grid.move(to: CGPoint(x: 0, y: sz.height * f)); grid.addLine(to: CGPoint(x: sz.width, y: sz.height * f))
                    ctx.stroke(grid, with: .color(.primary.opacity(0.09)), lineWidth: 1)
                }
                if let histogram {
                    let pts = HistogramPath.polygon(histogram, width: sz.width, height: sz.height)
                    ctx.fill(Path { $0.addLines(pts); $0.closeSubpath() }, with: .color(.primary.opacity(0.08)))
                }
                var diag = Path()
                diag.move(to: CGPoint(x: 0, y: sz.height)); diag.addLine(to: CGPoint(x: sz.width, y: 0))
                ctx.stroke(diag, with: .color(.primary.opacity(0.18)), style: StrokeStyle(lineWidth: 1, dash: [3, 3]))
                let line = HistogramPath.polyline(model.samples(count: 128), width: sz.width, height: sz.height)
                ctx.stroke(Path { $0.addLines(line) }, with: .color(color), lineWidth: 1.5)
                for (i, p) in model.points.enumerated() {
                    let c = CGPoint(x: p.x * sz.width, y: (1 - p.y) * sz.height)
                    let r: CGFloat = i == dragIndex ? 5 : 4
                    let dot = Path(ellipseIn: CGRect(x: c.x - r, y: c.y - r, width: 2 * r, height: 2 * r))
                    ctx.fill(dot, with: .color(i == dragIndex ? DS.accent : Color(white: 0.12)))
                    ctx.stroke(dot, with: .color(i == dragIndex ? DS.accent : color), lineWidth: 1.5)
                }
            }
            .contentShape(.rect)
            .gesture(DragGesture(minimumDistance: 0).onChanged { g in
                var m = working ?? CurveModel(stored: points)
                let p = curvePoint(g.location, size)
                if dragIndex == nil {
                    dragIndex = m.hitTest(p, radius: Self.hitRadius) ?? m.insert(p)
                }
                if let i = dragIndex {
                    m.move(i, to: p)
                }
                working = m
                // Live update unless the point is being dragged out for deletion.
                if !CurveModel.isDraggedOut(p) { onChange(m.stored) }
            }.onEnded { g in
                if var m = working, let i = dragIndex {
                    if CurveModel.isDraggedOut(curvePoint(g.location, size)), m.remove(i) { onChange(m.stored) }
                }
                dragIndex = nil
                working = nil
            })
            .simultaneousGesture(SpatialTapGesture(count: 2).onEnded { e in
                var m = CurveModel(stored: points)
                if let i = m.hitTest(curvePoint(e.location, size), radius: Self.hitRadius), m.remove(i) {
                    onChange(m.stored)
                }
            })
        }
        .aspectRatio(1, contentMode: .fit)
        .background(DS.plotBackground, in: .rect(cornerRadius: DS.Radius.s))
        .overlay(RoundedRectangle(cornerRadius: DS.Radius.s).strokeBorder(DS.hairline))
        .help("Click to add a point, drag to shape the curve, double-click or drag a point out to remove it")
    }

    private func curvePoint(_ loc: CGPoint, _ size: CGSize) -> CurvePoint {
        CurvePoint(Double(loc.x / max(size.width, 1)), Double(1 - loc.y / max(size.height, 1)))
    }
}
