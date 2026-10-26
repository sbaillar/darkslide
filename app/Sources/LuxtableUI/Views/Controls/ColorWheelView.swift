import LuxtableCore
import SwiftUI

/// One color-balance wheel: drag the puck for hue/saturation, lightness below.
/// Double-click the wheel to reset it.
struct ColorWheelView: View {
    let title: String
    let value: WheelValue
    var diameter: CGFloat = 104
    let onChange: (WheelValue, String) -> Void

    var body: some View {
        content
            .accessibilityElement(children: .contain)
            .accessibilityLabel(ControlDescriptions.wheel(title, value).label)
            .accessibilityValue(ControlDescriptions.wheel(title, value).value)
    }

    private var content: some View {
        VStack(spacing: DS.Space.xs) {
            let swatch = WheelMath.previewColor(value)
            HStack(spacing: DS.Space.xs) {
                Circle().fill(Color(red: swatch.r, green: swatch.g, blue: swatch.b)).frame(width: 7, height: 7)
                Text(title).font(DS.Font.label).foregroundStyle(value == WheelValue() ? DS.textSecondary : DS.text)
            }
            ZStack {
                Circle().fill(AngularGradient(
                    gradient: Gradient(colors: stride(from: 0.0, through: 360, by: 30).map { h in
                        let c = WheelMath.rgb(hue: h, saturation: 0.85, value: 0.9)
                        return Color(red: c.r, green: c.g, blue: c.b)
                    }),
                    center: .center, startAngle: .degrees(0), endAngle: .degrees(-360)))
                Circle().fill(RadialGradient(colors: [Color(white: 0.45), Color(white: 0.45).opacity(0)],
                                             center: .center, startRadius: 0, endRadius: diameter / 2))
                Circle().strokeBorder(Color.black.opacity(0.4), lineWidth: 1)
                Rectangle().fill(.black.opacity(0.25)).frame(width: 1, height: diameter * 0.12)
                Rectangle().fill(.black.opacity(0.25)).frame(width: diameter * 0.12, height: 1)
                let p = WheelMath.point(hue: value.hue, saturation: value.saturation, size: diameter)
                Circle()
                    .strokeBorder(Color.white, lineWidth: 2)
                    .background(Circle().fill(Color(red: swatch.r, green: swatch.g, blue: swatch.b)))
                    .frame(width: 12, height: 12)
                    .shadow(color: .black.opacity(0.5), radius: 2)
                    .position(p)
            }
            .frame(width: diameter, height: diameter)
            .contentShape(Circle())
            .gesture(DragGesture(minimumDistance: 0).onChanged { g in
                let v = WheelMath.value(at: g.location, size: diameter)
                var w = value
                w.hue = v.hue
                w.saturation = v.saturation
                onChange(w, "\(title) wheel")
            })
            .simultaneousGesture(TapGesture(count: 2).onEnded { onChange(WheelValue(), "Reset \(title)") })
            .help("Drag to tint the \(title.lowercased()); double-click to reset")
            // M11: hue with the VoiceOver adjust gesture, saturation and reset as named actions.
            .accessibilityElement()
            .accessibilityLabel(ControlDescriptions.wheelHue(title, value).label)
            .accessibilityValue(ControlDescriptions.wheelHue(title, value).value)
            .accessibilityAdjustableAction { dir in
                onChange(ControlAdjust.wheelHue(value, steps: dir == .increment ? 1 : -1), "\(title) wheel")
            }
            .accessibilityAction(named: "More saturation") {
                onChange(ControlAdjust.wheelSaturation(value, steps: 1), "\(title) wheel")
            }
            .accessibilityAction(named: "Less saturation") {
                onChange(ControlAdjust.wheelSaturation(value, steps: -1), "\(title) wheel")
            }
            .accessibilityAction(named: "Reset") { onChange(WheelValue(), "Reset \(title)") }

            SliderTrack(spec: Specs.wheelLightness, value: value.lightness, tint: DS.accent, gradient: nil,
                        accessibilityName: "\(title) lightness") { l in
                var w = value
                w.lightness = l
                onChange(w, "\(title) lightness")
            }
            .frame(width: diameter, height: 14)
            .help("\(title) lightness; double-click to reset")
        }
    }
}
