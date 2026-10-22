import DarkslideCore
import SwiftUI

/// Remove (content-aware fill) in the Heal & Clone panel: paint over a distraction and
/// release. Shows the LaMa model's download (progress, Cancel, Try Again).
struct RemoveToolSection: View {
    @Bindable var model: AppModel
    private var engine: RemovalModel { RemovalModel.shared }

    var body: some View {
        let on = model.colorTools.mode == .remove
        let count = model.focusedEdit.removals.count
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack(spacing: DS.Space.xs) {
                Button {
                    model.colorTools.mode = on ? nil : .remove
                } label: {
                    Label("Remove", systemImage: "wand.and.rays")
                        .font(DS.Font.caption)
                        .padding(.horizontal, DS.Space.s)
                        .padding(.vertical, 3)
                        .background(on ? DS.accent.opacity(0.2) : Color.primary.opacity(0.05), in: .rect(cornerRadius: DS.Radius.s))
                        .foregroundStyle(on ? DS.accent : DS.text)
                }
                .buttonStyle(.plain)
                .help("Remove: paint over a distraction; it is filled from its surroundings")
                .accessibilityLabel(on ? "Stop remove tool" : "Remove tool")
                Spacer()
                if count > 0 {
                    Button("Undo Last Removal") { model.removeLastRemoval() }
                        .controlSize(.small)
                }
            }
            if on {
                ToolSlider(spec: RemoveToolSpecs.size, value: model.colorTools.removal.size) { v in
                    model.colorTools.removal.size = v
                }
            }
            if case .downloading(let f) = engine.model.state {
                HStack(alignment: .bottom, spacing: DS.Space.s) {
                    ProgressView(value: f) { Text(engine.status).font(DS.Font.caption) }
                        .controlSize(.small)
                    Button("Cancel") { engine.cancel() }
                        .controlSize(.small)
                        .accessibilityLabel("Cancel the removal model download")
                }
            } else if case .failed = engine.model.state {
                HStack {
                    PanelNote(text: engine.status)
                    Button("Try Again") { engine.activate() }.controlSize(.small)
                }
            } else if on || count > 0 {
                PanelNote(text: on ? "Paint over what to remove, then release. Esc to stop. " + engine.status
                          : "\(count) removal\(count == 1 ? "" : "s") on this photo.")
            }
        }
        .onDisappear {
            if model.colorTools.mode == .remove { model.colorTools.cancel() }
        }
    }
}

enum RemoveToolSpecs {
    static let size = SliderSpec("Size", RemoveToolState.sizeRange, default: 0.04, step: 0.005, format: .percent,
                                 help: "Brush diameter, as a share of the long edge ([ and ]).")
}
