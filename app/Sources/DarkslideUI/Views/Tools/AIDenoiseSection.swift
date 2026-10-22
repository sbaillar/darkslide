import DarkslideCore
import SwiftUI

/// AI Denoise (top of the Noise Reduction panel): on/off, strength, and the model's and
/// the focused photo's progress.
struct AIDenoiseSection: View {
    @Bindable var model: AppModel
    var service = DenoiseService.shared

    var body: some View {
        let noise = model.focusedEdit.noise
        let url = model.focusedPhoto?.url
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Toggle("AI Denoise", isOn: Binding(
                get: { noise.aiDenoise },
                set: { on in
                    model.updateEdit(label: "AI Denoise") { $0.noise.aiDenoise = on }
                    if on { Task { await service.checkFocused() } }
                }))
                .font(DS.Font.label)
                .disabled(model.focusedPhoto.map { $0.isVideo } ?? true || service.model.unavailableReason != nil)
                .help("Remove noise with a neural network on this Mac (Neural Engine and GPU)")
                .aiHighlight(model, AIFieldMap.aiDenoise, value: noise.aiDenoise ? "on" : "off")
            if let reason = service.model.unavailableReason {
                PanelNote(text: reason)
            } else if noise.aiDenoise {
                EditSlider(model: model, spec: Specs.aiDenoiseStrength, keyPath: \.noise.aiStrength)
                status(url)
            }
            if model.selectedIDs.count > 1, service.model.unavailableReason == nil {
                Button("Denoise \(model.selectedIDs.count) Selected") { service.denoiseSelected() }
                    .controlSize(.small)
                    .help("Turn AI Denoise on for the selected photos and denoise them in the background")
            }
        }
        .task(id: TaskKey(photo: model.focusedID, on: noise.aiDenoise)) {
            if noise.aiDenoise { await service.checkFocused() }
        }
    }

    private struct TaskKey: Hashable {
        let photo: PhotoID?
        let on: Bool
    }

    @ViewBuilder
    private func status(_ url: URL?) -> some View {
        if let url, let state = service.states[url] {
            switch state {
            case .queued:
                PanelNote(text: "Waiting to denoise…")
            case .running(let f):
                HStack(alignment: .bottom, spacing: DS.Space.s) {
                    ProgressView(value: f) { Text("Denoising…").font(DS.Font.caption) }
                        .controlSize(.small)
                    Button("Cancel") { service.cancel(url) }
                        .controlSize(.small)
                        .accessibilityLabel("Stop denoising this photo")
                }
            case .ready:
                PanelNote(text: "Denoised on this Mac.")
            case .failed(let message):
                HStack(spacing: DS.Space.s) {
                    Label(message, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.reject)
                    Button("Retry") { service.retry(url) }.controlSize(.small)
                }
            }
        }
    }
}
