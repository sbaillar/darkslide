import SwiftUI

/// Live RGB histogram: additive R/G/B fills plus a luma outline, with shadow
/// and highlight clipping indicators in the top corners.
struct HistogramView: View {
    let histogram: Histogram?
    /// "Histogram", or "Preview histogram" when the viewer shows the embedded JPEG.
    var title: String = "Histogram"
    var height: CGFloat = 120
    var showsClipping = true

    var body: some View {
        ZStack(alignment: .top) {
            Canvas { ctx, size in
                // Quarter grid.
                for i in 1..<4 {
                    let x = size.width * CGFloat(i) / 4
                    ctx.stroke(Path { $0.move(to: CGPoint(x: x, y: 0)); $0.addLine(to: CGPoint(x: x, y: size.height)) },
                               with: .color(.primary.opacity(0.08)), lineWidth: 1)
                }
                guard let h = histogram else { return }
                ctx.blendMode = .plusLighter
                let channels: [(Histogram.Channel, Color)] = [
                    (.red, Color(red: 0.95, green: 0.25, blue: 0.25)),
                    (.green, Color(red: 0.25, green: 0.85, blue: 0.35)),
                    (.blue, Color(red: 0.30, green: 0.45, blue: 1.0)),
                ]
                for (c, color) in channels {
                    let pts = HistogramPath.polygon(h.normalized(c), width: size.width, height: size.height)
                    ctx.fill(Path { $0.addLines(pts); $0.closeSubpath() }, with: .color(color.opacity(0.42)))
                }
                ctx.blendMode = .normal
                let luma = HistogramPath.polygon(h.normalized(.luma), width: size.width, height: size.height)
                ctx.stroke(Path { $0.addLines(Array(luma.dropFirst().dropLast())) },
                           with: .color(.primary.opacity(0.7)), lineWidth: 1)
            }
            if showsClipping, let h = histogram {
                HStack {
                    ClipIndicator(active: h.clipsShadows, color: .blue, pointsLeft: true)
                        .help(String(format: "Shadow clipping: %.2f%% of pixels are pure black in a channel", h.shadowClipFraction * 100))
                    Spacer()
                    ClipIndicator(active: h.clipsHighlights, color: .red, pointsLeft: false)
                        .help(String(format: "Highlight clipping: %.2f%% of pixels are pure white in a channel", h.highlightClipFraction * 100))
                }
                .padding(DS.Space.xs)
            }
            if title != "Histogram" {
                Text(title.uppercased())
                    .font(.system(size: 8, weight: .semibold))
                    .tracking(0.5)
                    .foregroundStyle(DS.textTertiary)
                    .padding(.top, 3)
                    .help("The viewer shows the camera's embedded JPEG, so this histogram doesn't reflect your edits yet.")
            }
            if histogram == nil {
                Text("No image")
                    .font(DS.Font.caption)
                    .foregroundStyle(DS.textTertiary)
                    .frame(maxHeight: .infinity)
            }
        }
        .frame(height: height)
        .background(DS.plotBackground, in: .rect(cornerRadius: DS.Radius.m))
        .overlay(RoundedRectangle(cornerRadius: DS.Radius.m).strokeBorder(DS.hairline))
        .clipShape(.rect(cornerRadius: DS.Radius.m))
        .help(title == "Histogram" ? "Histogram of the rendered image (red, green, blue, and luminance)"
              : "Histogram of the embedded preview (edits aren't rendered yet)")
        .accessibilityLabel(title)
        .accessibilityValue(HistogramText.value(histogram)) // [cul9] L6
    }
}

/// Small corner triangle that lights up when a clip threshold is exceeded.
struct ClipIndicator: View {
    let active: Bool
    let color: Color
    let pointsLeft: Bool

    var body: some View {
        Path { p in
            if pointsLeft {
                p.move(to: .zero); p.addLine(to: CGPoint(x: 9, y: 0)); p.addLine(to: CGPoint(x: 0, y: 9))
            } else {
                p.move(to: CGPoint(x: 9, y: 0)); p.addLine(to: .zero); p.addLine(to: CGPoint(x: 9, y: 9))
            }
            p.closeSubpath()
        }
        .fill(active ? color : Color.primary.opacity(0.2))
        .frame(width: 9, height: 9)
        .shadow(color: active ? color.opacity(0.8) : .clear, radius: 3)
    }
}

/// Under the histogram: shadow/highlight clipping percentages and, while the
/// pointer is over the image, its RGB (8-bit) and L*a*b* values.
struct HistogramFooter: View {
    let histogram: Histogram?
    /// Pixel under the viewer cursor (observed separately from the viewer).
    let readout: CursorReadout

    var body: some View {
        HStack(spacing: DS.Space.s) {
            Label(HistogramReadout.clip(histogram?.shadowClipFraction ?? 0), systemImage: "arrowtriangle.left.fill")
                .foregroundStyle(histogram?.clipsShadows == true ? Color.blue : DS.textSecondary)
                .help("Pixels with a channel at pure black")
            Spacer(minLength: 0)
            if let c = readout.rgb {
                let v = HistogramReadout.rgb(c.x, c.y, c.z)
                HStack(spacing: DS.Space.xs) {
                    Text("R").foregroundStyle(.red); Text(v[0])
                    Text("G").foregroundStyle(.green); Text(v[1])
                    Text("B").foregroundStyle(.blue); Text(v[2])
                }
                .help(readout.labText ?? "")
            } else {
                Text("Point at the image for RGB").foregroundStyle(DS.textTertiary)
            }
            Spacer(minLength: 0)
            Label(HistogramReadout.clip(histogram?.highlightClipFraction ?? 0), systemImage: "arrowtriangle.right.fill")
                .labelStyle(TrailingIconLabelStyle())
                .foregroundStyle(histogram?.clipsHighlights == true ? Color.red : DS.textSecondary)
                .help("Pixels with a channel at pure white")
        }
        .font(DS.Font.value)
        .foregroundStyle(DS.text)
        .lineLimit(1)
    }
}

struct TrailingIconLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 3) {
            configuration.title
            configuration.icon
        }
    }
}
