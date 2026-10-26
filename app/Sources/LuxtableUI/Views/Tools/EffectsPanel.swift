import LuxtableCore
import SwiftUI

/// Effects: lens blur (AI depth or a tilt-shift band), glow, grain and halation.
struct EffectsPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        let fx = model.focusedEdit.effects
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Text("LENS BLUR").font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
            Picker("Mode", selection: Binding(get: { fx.lensBlur.mode }, set: { m in
                model.updateEdit(label: "Lens Blur Mode") { $0.effects.lensBlur.mode = m }
                if m != .depth { model.focusPickerActive = false } // its button is gone
            })) {
                ForEach(BlurMode.pickable, id: \.self) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .help("Depth: blur by the photo's AI depth map. Tilt-Shift: a sharp band, blurred above and below.")
            if fx.lensBlur.mode == .newer {
                PanelNote(text: "This blur uses a mode from a newer version of Luxtable: it shows no blur here. Pick Depth or Tilt-Shift to replace it.")
            }
            EditSlider(model: model, spec: Specs.blurAmount, keyPath: \.effects.lensBlur.amount)
            if fx.lensBlur.mode == .depth {
                EditSlider(model: model, spec: Specs.focalRange, keyPath: \.effects.lensBlur.focalRange)
                focusRow(fx.lensBlur)
                depthModelRow
            } else if fx.lensBlur.mode == .plane {
                EditSlider(model: model, spec: Specs.bandPosition, keyPath: \.effects.lensBlur.band.centerY)
                EditSlider(model: model, spec: Specs.bandAngle, keyPath: \.effects.lensBlur.band.angleDeg)
                EditSlider(model: model, spec: Specs.bandWidth, keyPath: \.effects.lensBlur.band.width)
                EditSlider(model: model, spec: Specs.bandFeather, keyPath: \.effects.lensBlur.band.feather)
            }
            EditSlider(model: model, spec: Specs.highlightBoost, keyPath: \.effects.lensBlur.highlightBoost)
            Picker("Shape", selection: Binding(get: { fx.lensBlur.shape }, set: { s in
                model.updateEdit(label: "Bokeh Shape") { $0.effects.lensBlur.shape = s }
            })) {
                ForEach(BokehShape.allCases, id: \.self) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .help("Shape of the out-of-focus highlights: round, six-bladed, or cat's eyes swirling toward the corners")
            Divider()
            Text("GLOW").font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
            EditSlider(model: model, spec: Specs.glowAmount, keyPath: \.effects.glow.amount)
            EditSlider(model: model, spec: Specs.glowSize, keyPath: \.effects.glow.size)
            Text("GRAIN").font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
            EditSlider(model: model, spec: Specs.grainAmount, keyPath: \.effects.grain.amount)
            EditSlider(model: model, spec: Specs.grainSize, keyPath: \.effects.grain.size)
            EditSlider(model: model, spec: Specs.grainRoughness, keyPath: \.effects.grain.roughness)
            EditSlider(model: model, spec: Specs.halationAmount, keyPath: \.effects.halation.amount)
        }
        .onDisappear { model.focusPickerActive = false }
    }

    private func focusRow(_ lb: LensBlurEdit) -> some View {
        HStack(spacing: DS.Space.xs) {
            Chip(title: model.focusPickerActive ? "Done" : "Pick Focus", selected: model.focusPickerActive) {
                model.toggleFocusPicker()
            }
            .help("Click the photo where it should be sharp")
            .accessibilityLabel(model.focusPickerActive ? "Stop picking the focus" : "Pick the focus on the photo")
            Chip(title: "Focus on Subject", selected: lb.focus == nil) {
                model.updateEdit(label: "Lens Blur Focus") { $0.effects.lensBlur.focus = nil }
            }
            .help("Keep the main subject sharp (found by Vision)")
            Spacer(minLength: 0)
        }
    }

    @ViewBuilder private var depthModelRow: some View {
        let depth = DepthMapService.shared
        switch depth.model.state {
        case .downloading(let f):
            HStack(alignment: .bottom, spacing: DS.Space.s) {
                ProgressView(value: f) { Text(depth.status).font(DS.Font.caption) }.controlSize(.small)
                Button("Cancel") { depth.model.cancel() }.controlSize(.small)
                    .accessibilityLabel("Cancel the depth model download")
            }
        case .notDownloaded, .failed:
            HStack {
                PanelNote(text: depth.status)
                Button("Download") { model.downloadDepthModel() }.controlSize(.small)
                    .help("Download the 50 MB depth model (Depth Anything V2 Small) for depth-accurate blur")
            }
        case .ready:
            PanelNote(text: depth.status)
        }
    }
}

extension AppModel {
    /// The lens blur's focus picker on the photo (the WB picker goes off).
    public func toggleFocusPicker() {
        focusPickerActive.toggle()
        if focusPickerActive { wbPickerActive = false }
    }
}
