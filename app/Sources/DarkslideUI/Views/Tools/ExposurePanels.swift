import DarkslideCore
import SwiftUI

struct ExposurePanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: DS.Space.s) {
            EditSlider(model: model, spec: Specs.exposure, keyPath: \.exposure.ev)
            EditSlider(model: model, spec: Specs.contrast, keyPath: \.exposure.contrast)
            EditSlider(model: model, spec: Specs.brightness, keyPath: \.exposure.brightness)
            EditSlider(model: model, spec: Specs.saturation, keyPath: \.exposure.saturation)
        }
    }
}

struct HDRPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: DS.Space.s) {
            EditSlider(model: model, spec: Specs.highlights, keyPath: \.hdr.highlights)
            EditSlider(model: model, spec: Specs.shadows, keyPath: \.hdr.shadows)
            EditSlider(model: model, spec: Specs.whites, keyPath: \.hdr.whites)
            EditSlider(model: model, spec: Specs.blacks, keyPath: \.hdr.blacks)
        }
    }
}

struct LevelsPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        LevelsView(levels: model.focusedEdit.levels, histogram: model.viewer.histogram?.normalized(.luma),
                   aiChanged: model.aiNote(AIFieldMap.levels) != nil) { l, key in
            model.updateEdit(label: "Levels", coalesceKey: key) { $0.levels = l }
        }
        .endsEditGesture(model)
        .modifier(AIHighlightRow(note: model.aiNote(AIFieldMap.levels), undo: model.aiUndo(AIFieldMap.levels)))
    }
}

struct CurvePanel: View {
    @Bindable var model: AppModel
    @State private var channel: CurveChannel = .rgb

    var body: some View {
        let curve = model.focusedEdit.curve
        VStack(spacing: DS.Space.s) {
            Picker("Channel", selection: $channel) {
                ForEach(CurveChannel.allCases, id: \.self) { c in
                    Text(c == .rgb ? "RGB" : c == .luma ? "Luma" : String(c.rawValue.prefix(1))).tag(c)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .help("Curve channel: RGB, luminance only, or a single color channel")
            CurveEditorView(points: curve.points(for: channel), color: color(channel),
                            histogram: model.viewer.histogram?.normalized(histogramChannel(channel))) { pts in
                let ch = channel
                model.updateEdit(label: "Curve", coalesceKey: "curve-\(ch.rawValue)") { $0.curve.points[ch] = pts.isEmpty ? nil : pts }
            }
            .endsEditGesture(model)
            .aiHighlight(model, AIFieldMap.curve)
        }
    }

    private func color(_ c: CurveChannel) -> Color {
        switch c {
        case .rgb, .luma: .primary
        case .red: Color(red: 1, green: 0.4, blue: 0.4)
        case .green: Color(red: 0.4, green: 0.9, blue: 0.5)
        case .blue: Color(red: 0.45, green: 0.6, blue: 1)
        }
    }

    private func histogramChannel(_ c: CurveChannel) -> Histogram.Channel {
        switch c {
        case .rgb, .luma: .luma
        case .red: .red
        case .green: .green
        case .blue: .blue
        }
    }
}
