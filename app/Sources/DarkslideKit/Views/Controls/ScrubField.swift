import AppKit
import SwiftUI

/// Numeric value that scrubs on horizontal drag (⌥ = fine) and becomes a
/// text field on click. Math lives in `ScrubMath` / `SliderSpec`.
struct ScrubField: View {
    let spec: SliderSpec
    let value: Double
    var onEnd: () -> Void = {}
    let onChange: (Double) -> Void

    @State private var dragStart: Double?
    @State private var editing = false
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        content
            .accessibilityLabel("\(spec.label) value")
            .accessibilityValue(spec.string(value))
    }

    private var content: some View {
        Group {
            if editing {
                TextField("", text: $text)
                    .textFieldStyle(.plain)
                    .font(DS.Font.value)
                    .multilineTextAlignment(.trailing)
                    .focused($focused)
                    .onSubmit(commit)
                    .onExitCommand { editing = false }
                    .onChange(of: focused) { _, f in if !f { commit() } }
                    .onAppear { focused = true }
            } else {
                Text(spec.string(value))
                    .font(DS.Font.value)
                    .foregroundStyle(spec.isDefault(value) ? DS.textSecondary : DS.text)
                    .frame(maxWidth: .infinity, alignment: .trailing)
                    .contentShape(.rect)
                    .gesture(DragGesture(minimumDistance: 2).onChanged { g in
                        let start = dragStart ?? value
                        if dragStart == nil { dragStart = value }
                        let fine = NSEvent.modifierFlags.contains(.option)
                        onChange(ScrubMath.value(start: start, dx: g.translation.width, spec: spec, fine: fine))
                    }.onEnded { _ in
                        dragStart = nil
                        onEnd()
                    })
                    .onTapGesture {
                        text = spec.string(value)
                        editing = true
                    }
                    .pointerStyle(.columnResize)
                    .help("Drag to scrub (⌥ for fine), click to type")
            }
        }
        .padding(.horizontal, DS.Space.xs)
        .frame(height: 18)
        .background(Color.primary.opacity(editing ? 0.12 : 0.05), in: .rect(cornerRadius: DS.Radius.s))
    }

    private func commit() {
        if let v = spec.parse(text) {
            onChange(v)
            onEnd()
        }
        editing = false
    }
}
