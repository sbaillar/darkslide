import LuxtableCore
import SwiftUI

/// Levels: histogram with input black / gamma / white handles below it, and an
/// output ramp with black / white handles. Math in `LevelsMath`.
struct LevelsView: View {
    let levels: LevelsEdit
    let histogram: [Double]?
    /// An AI edit set these levels (VoiceOver says so after the value).
    var aiChanged = false
    let onChange: (LevelsEdit, String) -> Void

    private enum Handle { case inBlack, mid, inWhite, outBlack, outWhite }

    var body: some View {
        content
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(ControlDescriptions.levels(levels).label)
            .accessibilityValue(aiChanged ? AIHighlightStyle.accessibilityValue(ControlDescriptions.levels(levels).value)
                                : ControlDescriptions.levels(levels).value)
            // M11: gamma with the adjust gesture; black and white points as named actions.
            .accessibilityAdjustableAction { dir in
                onChange(ControlAdjust.levelsGamma(levels, steps: dir == .increment ? 1 : -1), "Levels gamma")
            }
            .accessibilityAction(named: "Raise black point") {
                onChange(ControlAdjust.levelsInput(levels, black: true, steps: 1), "Levels black")
            }
            .accessibilityAction(named: "Lower black point") {
                onChange(ControlAdjust.levelsInput(levels, black: true, steps: -1), "Levels black")
            }
            .accessibilityAction(named: "Raise white point") {
                onChange(ControlAdjust.levelsInput(levels, black: false, steps: 1), "Levels white")
            }
            .accessibilityAction(named: "Lower white point") {
                onChange(ControlAdjust.levelsInput(levels, black: false, steps: -1), "Levels white")
            }
    }

    private var content: some View {
        VStack(spacing: DS.Space.xs) {
            GeometryReader { geo in
                let w = geo.size.width
                ZStack(alignment: .topLeading) {
                    Canvas { ctx, size in
                        if let histogram {
                            let pts = HistogramPath.polygon(histogram, width: size.width, height: size.height)
                            ctx.fill(Path { $0.addLines(pts); $0.closeSubpath() }, with: .color(.primary.opacity(0.28)))
                        }
                        // Transfer curve preview.
                        let samples = (0..<64).map { LevelsMath.transfer(Double($0) / 63, levels) }
                        let line = HistogramPath.polyline(samples, width: size.width, height: size.height)
                        ctx.stroke(Path { $0.addLines(line) }, with: .color(DS.accent.opacity(0.8)), lineWidth: 1)
                        // Clipped regions outside the input range.
                        let shade = Color.black.opacity(0.35)
                        ctx.fill(Path(CGRect(x: 0, y: 0, width: levels.inBlack * size.width, height: size.height)), with: .color(shade))
                        ctx.fill(Path(CGRect(x: levels.inWhite * size.width, y: 0,
                                             width: (1 - levels.inWhite) * size.width, height: size.height)), with: .color(shade))
                    }
                    .frame(height: 64)
                    // M10: the histogram's plot styling, so light mode isn't a muddy grey slab.
                    .background(DS.plotBackground, in: .rect(cornerRadius: DS.Radius.s))
                    .overlay(RoundedRectangle(cornerRadius: DS.Radius.s).strokeBorder(DS.hairline))

                    handle(.inBlack, x: levels.inBlack * w, y: 66, fill: .black, width: w)
                    handle(.mid, x: LevelsMath.midpoint(levels) * w, y: 66, fill: Color(white: 0.5), width: w)
                    handle(.inWhite, x: levels.inWhite * w, y: 66, fill: .white, width: w)

                    LinearGradient(colors: [.black, .white], startPoint: .leading, endPoint: .trailing)
                        .frame(height: 6)
                        .clipShape(.capsule)
                        .offset(y: 84)
                    handle(.outBlack, x: levels.outBlack * w, y: 92, fill: .black, width: w)
                    handle(.outWhite, x: levels.outWhite * w, y: 92, fill: .white, width: w)
                }
                .coordinateSpace(.named("levels"))
            }
            .frame(height: 104)
            HStack {
                Text("In \(Safe.clampedInt(levels.inBlack * 255)) · \(String(format: "%.2f", levels.gamma)) · \(Safe.clampedInt(levels.inWhite * 255))")
                Spacer()
                Text("Out \(Safe.clampedInt(levels.outBlack * 255)) · \(Safe.clampedInt(levels.outWhite * 255))")
            }
            .font(DS.Font.value)
            .foregroundStyle(DS.textSecondary)
        }
    }

    private func handle(_ h: Handle, x: Double, y: CGFloat, fill: Color, width: CGFloat) -> some View {
        Triangle()
            .fill(fill)
            .overlay(Triangle().stroke(Color.white.opacity(0.7), lineWidth: 0.75))
            .frame(width: 10, height: 9)
            .offset(x: x - 5, y: y)
            .contentShape(.rect.inset(by: -4))
            .gesture(DragGesture(minimumDistance: 0, coordinateSpace: .named("levels")).onChanged { g in
                let v = Double(g.location.x / max(width, 1))
                switch h {
                case .inBlack: onChange(LevelsMath.settingInBlack(levels, v), "Levels input black")
                case .mid: onChange(LevelsMath.settingMidpoint(levels, v), "Levels midtone")
                case .inWhite: onChange(LevelsMath.settingInWhite(levels, v), "Levels input white")
                case .outBlack: onChange(LevelsMath.settingOutBlack(levels, v), "Levels output black")
                case .outWhite: onChange(LevelsMath.settingOutWhite(levels, v), "Levels output white")
                }
            })
            .simultaneousGesture(TapGesture(count: 2).onEnded {
                let d = LevelsEdit()
                var l = levels
                switch h {
                case .inBlack: l.inBlack = d.inBlack
                case .mid: l.gamma = d.gamma
                case .inWhite: l.inWhite = d.inWhite
                case .outBlack: l.outBlack = d.outBlack
                case .outWhite: l.outWhite = d.outWhite
                }
                onChange(l, "Reset levels handle")
            })
            .help(help(h))
    }

    private func help(_ h: Handle) -> String {
        switch h {
        case .inBlack: "Input black point: everything darker becomes black. Double-click to reset."
        case .mid: "Midtones (gamma). Double-click to reset."
        case .inWhite: "Input white point: everything brighter becomes white. Double-click to reset."
        case .outBlack: "Output black: lift for a faded look. Double-click to reset."
        case .outWhite: "Output white: lower to soften highlights. Double-click to reset."
        }
    }
}

/// Upward-pointing triangle (handle marker).
struct Triangle: Shape {
    func path(in r: CGRect) -> Path {
        Path { p in
            p.move(to: CGPoint(x: r.midX, y: r.minY))
            p.addLine(to: CGPoint(x: r.maxX, y: r.maxY))
            p.addLine(to: CGPoint(x: r.minX, y: r.maxY))
            p.closeSubpath()
        }
    }
}
