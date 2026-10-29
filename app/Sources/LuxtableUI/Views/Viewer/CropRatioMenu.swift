import LuxtableCore
import SwiftUI

/// The stage bar's Crop: clicking it enters the crop tool and drops down the ratios; its ▾
/// opens them again while cropping. Clicking Crop while cropping applies and leaves the tool.
struct CropToolButton: View {
    @Bindable var model: AppModel

    var body: some View {
        let on = model.viewer.canvasTool == .crop
        HStack(spacing: 0) {
            Button {
                if !on { model.cropRatiosShown = true }
                model.perform(.tool(.crop))
            } label: {
                Label(CanvasTool.crop.viewerLabel, systemImage: CanvasTool.crop.viewerSymbol)
            }
            .buttonStyle(ToolbarPillStyle(showsTitle: true, isOn: on))
            .help(ToolbarHelp.tool(.crop, model.settings.keyBindings))
            .accessibilityLabel("\(CanvasTool.crop.viewerLabel) tool")
            Button { model.cropRatiosShown.toggle() } label: {
                Label("Crop ratio", systemImage: "chevron.down")
            }
            .buttonStyle(ToolbarPillStyle())
            .help("Crop ratio: \(model.settings.cropRatio.title)")
            .accessibilityLabel("Crop ratio")
            .accessibilityValue(model.settings.cropRatio.title)
        }
        .disabled(model.focusedPhoto.map { $0.isVideo } ?? true)
        .popover(isPresented: $model.cropRatiosShown, arrowEdge: .bottom) {
            CropRatioList(model: model) { model.cropRatiosShown = false }
        }
    }
}

/// The crop ratios, the chosen one highlighted; picking one keeps it for every later crop.
struct CropRatioList: View {
    @Bindable var model: AppModel
    var done: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("CROP RATIO").font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
                .padding(.horizontal, DS.Space.s).padding(.bottom, DS.Space.xs)
            ForEach(CropRatio.allCases) { r in
                let selected = model.settings.cropRatio == r
                Button {
                    model.chooseCropRatio(r)
                    done()
                } label: {
                    HStack(spacing: DS.Space.s) {
                        Image(systemName: "checkmark").font(.system(size: 10, weight: .bold))
                            .opacity(selected ? 1 : 0).frame(width: 14)
                        Text(r.title).foregroundStyle(DS.text)
                        Spacer(minLength: DS.Space.m)
                        if let d = r.detail { Text(d).font(DS.Font.caption).foregroundStyle(DS.textSecondary) }
                    }
                    .padding(.horizontal, DS.Space.s).padding(.vertical, 5)
                    .background(selected ? DS.accent.opacity(0.22) : .clear, in: .rect(cornerRadius: 6))
                    .contentShape(.rect)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Crop ratio \(r.title)")
                .accessibilityAddTraits(selected ? .isSelected : [])
            }
            Divider().padding(.vertical, DS.Space.xs)
            Button {
                model.swapCropOrientation()
                done()
            } label: {
                HStack {
                    Image(systemName: "arrow.trianglehead.2.clockwise.rotate.90").frame(width: 14)
                    Text("Swap Orientation").foregroundStyle(DS.text)
                    Spacer()
                    Text("X").font(DS.Font.value).foregroundStyle(DS.textSecondary)
                }
                .padding(.horizontal, DS.Space.s).padding(.vertical, 5)
                .contentShape(.rect)
            }
            .buttonStyle(.plain)
            Text("Drag on the photo to draw a crop in this ratio.")
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                .padding(.horizontal, DS.Space.s).padding(.top, DS.Space.xs)
        }
        .font(.system(size: 12))
        .padding(DS.Space.s)
        .frame(width: 260)
    }
}
