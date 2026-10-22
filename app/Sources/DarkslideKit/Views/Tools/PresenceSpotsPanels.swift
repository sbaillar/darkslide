import SwiftUI

/// Vibrance, texture and dehaze (Lightroom's Presence).
struct PresencePanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: DS.Space.s) {
            EditSlider(model: model, spec: LayerToolSpecs.vibrance, keyPath: \.presence.vibrance)
            EditSlider(model: model, spec: LayerToolSpecs.texture, keyPath: \.presence.texture)
            EditSlider(model: model, spec: LayerToolSpecs.dehaze, keyPath: \.presence.dehaze)
        }
    }
}

/// Heal & Clone: a click heals a spot (the source is found automatically), a drag from
/// the spot sets where the source comes from.
struct SpotsPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        let spots = model.focusedEdit.spots
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack(spacing: DS.Space.xs) {
                toolButton(.heal, title: "Heal", symbol: "bandage")
                toolButton(.clone, title: "Clone", symbol: "square.on.square.dashed")
                Spacer()
                if !spots.isEmpty {
                    Button("Remove Last") {
                        model.updateEdit(label: "Remove Spot") { e in _ = e.spots.popLast() }
                    }
                    .controlSize(.small)
                }
            }
            ToolSlider(spec: LayerToolSpecs.spotSize, value: model.colorTools.spotRadius) { v in
                model.colorTools.spotRadius = v
            }
            if case .spot? = model.colorTools.mode {
                PanelNote(text: "Click a spot to fix it, or drag from the spot to where the fix should come from. Esc to stop.")
            } else {
                PanelNote(text: spots.isEmpty
                          ? "Remove dust and small distractions."
                          : "\(spots.count) spot\(spots.count == 1 ? "" : "s") on this photo.")
            }
        }
        .onDisappear {
            if case .spot? = model.colorTools.mode { model.colorTools.cancel() }
        }
        RemoveToolSection(model: model)
    }

    private func toolButton(_ kind: SpotKind, title: String, symbol: String) -> some View {
        let on = model.colorTools.mode == .spot(kind)
        return Button {
            model.colorTools.mode = on ? nil : .spot(kind)
        } label: {
            Label(title, systemImage: symbol)
                .font(DS.Font.caption)
                .padding(.horizontal, DS.Space.s)
                .padding(.vertical, 3)
                .background(on ? DS.accent.opacity(0.2) : Color.primary.opacity(0.05), in: .rect(cornerRadius: DS.Radius.s))
                .foregroundStyle(on ? DS.accent : DS.text)
        }
        .buttonStyle(.plain)
        .help(kind == .heal ? "Heal: blend texture from nearby" : "Clone: copy from another area")
        .accessibilityLabel(on ? "Stop \(title.lowercased()) tool" : "\(title) tool")
    }
}

/// Slider specs for presence, layer tools, brushes and spots.
public enum LayerToolSpecs {
    public static let vibrance = SliderSpec("Vibrance", -100...100, help: "Boost muted colours first, protecting skin.")
    public static let texture = SliderSpec("Texture", -100...100, help: "Medium-sized detail: + crisper, – smoother.")
    public static let dehaze = SliderSpec("Dehaze", -100...100, help: "Remove (+) or add (–) atmospheric haze.")
    public static let temperatureShift = SliderSpec("Temp", -3000...3000, step: 10, format: .signedInteger,
                                                    help: "Warm (+) or cool (–) this layer, in Kelvin.")
    public static let tintShift = SliderSpec("Tint", -150...150, help: "Green (–) to magenta (+) for this layer.")
    public static let sharpening = SliderSpec("Sharpen", -100...100, help: "Sharpen (+) or soften (–) under the mask.")
    public static let brushSize = SliderSpec("Size", 0.005...0.5, default: 0.05, step: 0.005, format: .percent,
                                             help: "Brush diameter, as a share of the long edge ([ and ]).")
    public static let brushHardness = SliderSpec("Hardness", 0...1, default: 0.5, step: 0.01, format: .percent,
                                                 help: "Hard (100 %) or soft brush edge.")
    public static let brushFlow = SliderSpec("Flow", 0.05...1, default: 1, step: 0.01, format: .percent,
                                             help: "How much each pass of the brush adds.")
    /// Up to ds-core `MAX_PLANE_FEATHER` (0.05 of the long edge): the pipeline caps it there
    /// so a feathered mask renders the same in every view and in exports.
    public static let maskFeather = SliderSpec("Feather", 0...0.05, step: 0.001, format: .percent,
                                               help: "Soften the whole mask's edge.")
    public static let spotSize = SliderSpec("Size", 0.005...0.2, default: 0.02, step: 0.005, format: .percent,
                                            help: "Spot radius, as a share of the long edge.")
    public static let lumaLow = SliderSpec("Darkest", 0...1, step: 0.01, format: .percent, help: "Tones below this are left out.")
    public static let lumaHigh = SliderSpec("Brightest", 0...1, default: 1, step: 0.01, format: .percent,
                                            help: "Tones above this are left out.")
    public static let lumaFalloff = SliderSpec("Falloff", 0...0.5, default: 0.1, step: 0.01, format: .percent,
                                               help: "How softly the range fades at both ends.")
}
