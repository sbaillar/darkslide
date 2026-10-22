import SwiftUI

/// Photo ▸ Merge to HDR… / Merge to Panorama…: a preview that follows the options,
/// then the merge with its progress. The state lives in `MergeModel`.
struct MergeSheet: View {
    @Bindable var model: MergeModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text(model.title).font(.headline).foregroundStyle(DS.text)
                .accessibilityAddTraits(.isHeader)
            preview
            options.disabled(model.isMerging)
            if let error = model.error {
                HStack(alignment: .firstTextBaseline, spacing: DS.Space.s) {
                    Image(systemName: "xmark.octagon").foregroundStyle(DS.reject).accessibilityHidden(true)
                    Text(error).font(DS.Font.label).foregroundStyle(DS.text).textSelection(.enabled)
                }
                .accessibilityElement(children: .combine)
                .accessibilityLabel("Failed: \(error)")
            }
            if let p = model.progress {
                ProgressView(value: p) { Text("Merging\u{2026}").font(DS.Font.label) }
                    .accessibilityLabel("Merging, \(Int(p * 100)) percent")
            }
            buttons
        }
        .padding(DS.Space.l)
        .frame(width: 560)
        .background(DS.panelBackground)
    }

    private var preview: some View {
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            ZStack {
                Rectangle().fill(Color.black.opacity(0.25))
                if let p = model.preview {
                    Image(decorative: p.image, scale: 1).resizable().aspectRatio(contentMode: .fit)
                        .opacity(model.isPreviewing ? 0.6 : 1)
                }
                if model.isPreviewing {
                    ProgressView().controlSize(.small).accessibilityLabel("Updating the preview")
                }
            }
            .frame(height: 300)
            .clipShape(RoundedRectangle(cornerRadius: 4))
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Merge preview")
            Text(model.previewSummary ?? " ").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
        }
    }

    @ViewBuilder private var options: some View {
        switch model.kind {
        case .hdr:
            Toggle("Auto align", isOn: $model.options.align)
                .accessibilityHint("Lines the exposures up before merging (for hand-held brackets)")
            Picker("Deghost", selection: $model.options.deghost) {
                ForEach(PhotoMergeOptions.Deghost.allCases, id: \.self) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .frame(maxWidth: 360)
            .accessibilityHint("Uses one exposure where something moved between the frames")
        case .panorama:
            Picker("Projection", selection: $model.options.projection) {
                ForEach(PhotoMergeOptions.Projection.allCases, id: \.self) { p in
                    Text(p == .auto ? model.autoProjectionTitle : p.title).tag(p)
                }
            }
            .frame(maxWidth: 300)
            Toggle("Auto crop", isOn: $model.options.autoCrop)
                .accessibilityHint("Crops away the empty edges of the stitched panorama")
        }
    }

    private var buttons: some View {
        HStack {
            Spacer()
            Button("Cancel", role: .cancel) { model.cancel() }
                .keyboardShortcut(.cancelAction)
            Button("Merge") { model.merge() }
                .keyboardShortcut(.defaultAction)
                .disabled(!model.canMerge)
        }
    }
}
