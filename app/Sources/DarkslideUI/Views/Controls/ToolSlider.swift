import DarkslideCore
import SwiftUI

/// Compact Capture One-style slider row: label · track · scrubbable value.
/// Double-click the label or the track to reset. ⌥-drag = fine, scroll wheel
/// (once focused by a click) the scroll wheel and ←/→ (⇧ ×10, ⌥ fine) nudge it.
struct ToolSlider: View {
    let spec: SliderSpec
    let value: Double
    var tint: Color = DS.accent
    /// Optional gradient painted in the track (e.g. Kelvin blue→amber).
    var trackGradient: Gradient?
    /// The gesture ended (mouse-up, reset, or nudges gone idle): one undo step per gesture.
    var onEnd: () -> Void = {}
    /// Why an AI edit changed this value (the row is highlighted; nil: it didn't).
    var aiNote: String?
    /// Undo AI Change (the row's context menu; nil: none).
    var aiUndo: (() -> Void)?
    let onChange: (Double) -> Void

    var body: some View {
        let note = aiNote.map { "\($0)\n\n" } ?? ""
        HStack(spacing: DS.Space.s) {
            HStack(spacing: DS.Space.xxs) {
                Text(spec.label)
                    .font(DS.Font.label)
                    .foregroundStyle(spec.isDefault(value) ? DS.textSecondary : DS.text)
                    .lineLimit(1)
                if aiNote != nil { AIBadge() }
            }
            .frame(width: 76, alignment: .leading)
            .contentShape(.rect)
            .onTapGesture(count: 2) {
                onChange(spec.resetValue)
                onEnd()
            }
            .help("\(note)\(spec.help) Double-click to reset.")
            // The track carries the label, value and actions: reading this one too would say them twice.
            .accessibilityHidden(true)
            SliderTrack(spec: spec, value: value, tint: tint, gradient: trackGradient, onEnd: onEnd,
                        aiChanged: aiNote != nil, onChange: onChange)
                .help("\(note)\(spec.help) ⌥-drag for fine control, scroll to adjust, click it, then scroll or ←/→ to nudge.")
            ScrubField(spec: spec, value: value, onEnd: onEnd, onChange: onChange)
                .frame(width: 58)
        }
        .frame(height: DS.Size.sliderRow)
        .modifier(AIHighlightRow(note: aiNote, badge: false, undo: aiUndo))
    }
}

/// The draggable track. Bipolar specs fill from the default outwards.
struct SliderTrack: View {
    let spec: SliderSpec
    let value: Double
    let tint: Color
    let gradient: Gradient?
    var onEnd: () -> Void = {}
    /// VoiceOver name when the spec's label is shared (e.g. each wheel's "Lightness").
    var accessibilityName: String?
    /// An AI edit changed the value (VoiceOver says so after it).
    var aiChanged = false
    let onChange: (Double) -> Void
    @State private var hovering = false
    @State private var idle = GestureIdleEnder()
    @State private var dragStart: (value: Double, x: Double)?
    @FocusState private var focused: Bool

    var body: some View {
        GeometryReader { geo in
            let w: CGFloat = geo.size.width
            let f = CGFloat(spec.fraction(value))
            let origin = CGFloat(spec.fraction(spec.defaultValue))
            let active = hovering || focused
            let markerWidth: CGFloat = active ? 3 : 2
            // A fine ruler: ticks hang from the top, a notch marks the default, a thin
            // amber line runs from the default to the value, a slim amber marker sits on it.
            ZStack(alignment: .leading) {
                if let gradient {
                    Capsule()
                        .fill(LinearGradient(gradient: gradient, startPoint: .leading, endPoint: .trailing))
                        .opacity(0.6)
                        .frame(height: 2)
                        .offset(y: 4)
                }
                RulerMarks(origin: Double(origin))
                Rectangle()
                    .fill(tint)
                    .frame(width: max(abs(f - origin) * w, 0), height: 1.5)
                    .offset(x: min(f, origin) * w, y: 1.5)
                    .opacity(spec.isDefault(value) ? 0 : 0.9)
                RoundedRectangle(cornerRadius: 1)
                    .fill(tint)
                    .shadow(color: .black.opacity(0.35), radius: 1, y: 0.5)
                    .overlay {
                        if focused { RoundedRectangle(cornerRadius: 2).strokeBorder(tint.opacity(0.45), lineWidth: 3).padding(-2.5) }
                    }
                    .frame(width: markerWidth, height: active ? 16 : 14)
                    .offset(x: f * w - markerWidth / 2)
            }
            .frame(maxHeight: .infinity)
            .contentShape(.rect)
            .gesture(DragGesture(minimumDistance: 0).onChanged { g in
                focused = true
                let fine = PlatformInput.fineAdjustActive
                if dragStart == nil { dragStart = (value, Double(g.startLocation.x)) }
                let start = dragStart ?? (value, Double(g.startLocation.x))
                onChange(SliderInput.trackDrag(startValue: start.value, startX: start.x, x: Double(g.location.x),
                                               width: Double(w), spec: spec, fine: fine))
            }.onEnded { g in
                dragStart = nil
                onEnd()
                // after a drag, ←/→ go back to culling (a click keeps focus to nudge).
                if !ControlFocus.keepsFocus(afterPointerMovedBy: hypot(g.translation.width, g.translation.height)) {
                    focused = false
                }
            })
            .simultaneousGesture(TapGesture(count: 2).onEnded {
                onChange(spec.resetValue)
                onEnd()
            })
            // Scroll adjusts only a focused slider, so scrolling the panel never changes values by accident.
            .background(ScrollWheelCatcher(enabled: focused) { dy, precise in
                let option = PlatformInput.fineAdjustActive
                onChange(SliderInput.scroll(value, deltaY: dy, precise: precise, spec: spec, option: option))
                idle.touch(onEnd)
            })
            .onHover { hovering = $0 }
            .animation(.snappy(duration: 0.12), value: hovering)
        }
        .focusable()
        .focusEffectDisabled()
        .focused($focused)
        .onChange(of: focused) { _, f in
            ControlFocus.shared.focusChanged(f)
            if !f { idle.flush() }
        }
        .onDisappear { if focused { ControlFocus.shared.focusChanged(false) } }
        .onChange(of: ControlFocus.shared.resignGeneration) { _, _ in focused = false }
        .onKeyPress(keys: [.leftArrow, .rightArrow, .upArrow, .downArrow]) { press in
            let up = press.key == .rightArrow || press.key == .upArrow
            onChange(SliderInput.nudge(value, up ? .up : .down, spec: spec, shift: press.modifiers.contains(.shift),
                                       option: press.modifiers.contains(.option)))
            idle.touch(onEnd)
            return .handled
        }
        .onKeyPress(.escape) {
            focused = false
            return .handled
        }
        .accessibilityElement()
        .accessibilityLabel(accessibilityName ?? spec.label)
        .accessibilityValue(aiChanged ? AIHighlightStyle.accessibilityValue(spec.string(value)) : spec.string(value))
        .accessibilityAdjustableAction { dir in
            onChange(SliderInput.nudge(value, dir == .increment ? .up : .down, spec: spec))
            idle.touch(onEnd)
        }
        .accessibilityAction(named: "Reset to default") {
            onChange(spec.resetValue)
            onEnd()
        }
    }
}

/// The slider's ruler: fine ticks hanging from the top and a notch at the default.
struct RulerMarks: View {
    /// The default value's position along the track (0...1).
    let origin: Double

    var body: some View {
        Canvas { ctx, size in Self.draw(&ctx, size: size, origin: CGFloat(origin)) }
            .allowsHitTesting(false)
            .accessibilityHidden(true)
    }

    private static func draw(_ ctx: inout GraphicsContext, size: CGSize, origin: CGFloat) {
        let mid: CGFloat = size.height / 2
        let right: CGFloat = max(0.5, size.width - 0.5)
        var minor = Path(), major = Path()
        for t in SliderRuler.ticks(width: Double(size.width)) {
            let x: CGFloat = min(max(CGFloat(t.fraction) * size.width, 0.5), right)
            let h: CGFloat = t.isMajor ? 6 : 3.5
            let tick = CGRect(x: x - 0.5, y: mid - 6, width: 1, height: h)
            if t.isMajor { major.addRect(tick) } else { minor.addRect(tick) }
        }
        ctx.fill(minor, with: .color(.primary.opacity(0.16)))
        ctx.fill(major, with: .color(.primary.opacity(0.3)))
        // Notch at the default: a small downward wedge on the top edge.
        let nx: CGFloat = origin * size.width
        let top: CGFloat = mid - 9
        var notch = Path()
        notch.move(to: CGPoint(x: nx - 3, y: top))
        notch.addLine(to: CGPoint(x: nx + 3, y: top))
        notch.addLine(to: CGPoint(x: nx, y: top + 3.5))
        notch.closeSubpath()
        ctx.fill(notch, with: .color(.primary.opacity(0.5)))
    }
}
