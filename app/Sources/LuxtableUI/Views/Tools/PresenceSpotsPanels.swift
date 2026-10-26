import LuxtableCore
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
