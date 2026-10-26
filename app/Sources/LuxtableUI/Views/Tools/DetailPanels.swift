import LuxtableCore
import SwiftUI

struct LensPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        let lens = model.focusedEdit.lens
        let info = model.focusedID.flatMap { model.developInfo[$0] }
        VStack(alignment: .leading, spacing: DS.Space.s) {
            // [tl8] The profile the pipeline actually applies (auto-detected from the lens EXIF).
            InfoRow(label: "Profile", value: LensReadout.profile(info: info, exifLens: model.focusedPhoto?.meta.lens,
                                                                 chosen: lens.profile))
            // Manual profile choice (e.g. a Sigma or Tamron lens with no automatic match).
            Menu {
                Button("Automatic (from the lens EXIF)") { model.updateEdit(label: "Lens Profile") { $0.lens.profile = nil } }
                ForEach(LensProfiles.grouped(), id: \.maker) { group in
                    Menu(group.maker) {
                        ForEach(group.models, id: \.self) { m in
                            Button(m) { model.updateEdit(label: "Lens Profile") { $0.lens.profile = m } }
                        }
                    }
                }
            } label: {
                Text(lens.profile == nil ? "Choose Profile…" : "Change Profile…").font(DS.Font.label)
            }
            .controlSize(.small)
            .help("Pick the correction profile by hand when the automatic match is wrong or missing")
            if let help = LensReadout.noMatchHelp(info: info, chosen: lens.profile) {
                PanelNote(text: help)
            }
            Toggle("Enable profile corrections", isOn: Binding(
                get: { lens.enabled },
                set: { v in model.updateEdit(label: "Lens Correction") { $0.lens.enabled = v } }))
                .font(DS.Font.label)
                .help("Correct distortion and corner shading using the lens profile")
                .aiHighlight(model, AIFieldMap.lensEnabled, value: lens.enabled ? "on" : "off")
            Group {
                EditSlider(model: model, spec: Specs.distortion, keyPath: \.lens.distortion)
                EditSlider(model: model, spec: Specs.lightFalloff, keyPath: \.lens.lightFalloff)
                Toggle("Remove chromatic aberration", isOn: Binding(
                    get: { lens.chromaticAberration },
                    set: { v in model.updateEdit(label: "Chromatic Aberration") { $0.lens.chromaticAberration = v } }))
                    .font(DS.Font.label)
                    .help("Remove color fringes along high-contrast edges")
                    .aiHighlight(model, AIFieldMap.chromaticAberration, value: lens.chromaticAberration ? "on" : "off")
            }
            .disabled(!lens.enabled)
            .opacity(lens.enabled ? 1 : 0.5)
            Text("PERSPECTIVE").font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
            EditSlider(model: model, spec: Specs.keystoneVertical, keyPath: \.lens.keystoneVertical)
            EditSlider(model: model, spec: Specs.keystoneHorizontal, keyPath: \.lens.keystoneHorizontal)
        }
        .task(id: model.focusedID) { if let id = model.focusedID { await model.loadDevelopInfo(for: [id]) } }
    }
}

struct VignettePanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: DS.Space.s) {
            EditSlider(model: model, spec: Specs.vignetteAmount, keyPath: \.vignette.amountEV)
            EditSlider(model: model, spec: Specs.vignetteMidpoint, keyPath: \.vignette.midpoint)
        }
    }
}

struct CropPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        let tool = model.viewer.canvasTool
        let preset = model.viewer.cropPreset
        let bindings = model.settings.keyBindings
        VStack(alignment: .leading, spacing: DS.Space.s) {
            // [cmp9] The canvas tools: crop (handles, thirds), straighten (drag a level line).
            HStack(spacing: DS.Space.xs) {
                Chip(title: "Crop", selected: tool == .crop) { model.perform(.tool(.crop)) }
                    .help(bindings.help("Crop: drag the handles; ⏎ applies, Esc cancels, X swaps orientation", .cropTool))
                    .accessibilityLabel("Crop tool")
                Chip(title: "Straighten", selected: tool == .straighten) { model.perform(.tool(.straighten)) }
                    .help(bindings.help("Straighten: drag along something that should be level or plumb", .straightenTool))
                    .accessibilityLabel("Straighten tool")
                Chip(title: "Swap", selected: false) { model.swapCropOrientation() }
                    .help("Swap the crop between portrait and landscape (X in the crop tool)")
                    .accessibilityLabel("Swap crop orientation")
            }
            Text("Aspect Ratio").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                .accessibilityValue(model.aiNote(AIFieldMap.cropFrame) == nil ? "" : AIHighlightStyle.accessibilityValue(""))
            LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: DS.Space.xs), count: 4), spacing: DS.Space.xs) {
                ForEach(CropPreset.chips) { p in
                    Chip(title: p.rawValue, selected: preset == p || (preset == .r9x16 && p == .r16x9)) {
                        model.setCropAspect(p)
                    }
                    .help(CropPanel.help(p))
                    .accessibilityLabel("Crop aspect \(p.rawValue)")
                }
            }
            .aiHighlight(model, AIFieldMap.cropFrame)
            let custom = CropRatioStore.shared.ratios
            if !custom.isEmpty {
                LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: DS.Space.xs), count: 4), spacing: DS.Space.xs) {
                    ForEach(custom) { r in
                        Chip(title: r.name, selected: false) { model.applyCropRatio(r) }
                            .help("Crop to \(r.name) (your ratio from Capture One; the crop stays free afterwards)")
                            .accessibilityLabel("Crop ratio \(r.name)")
                    }
                }
            }
            if tool == .crop {
                Text("⏎ apply · Esc cancel · X swap orientation")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
            EditSlider(model: model, spec: Specs.straighten, keyPath: \.crop.angle)
            Button {
                Task { await model.autoStraighten() }
            } label: {
                Label("Auto Straighten", systemImage: "level")
            }
            .controlSize(.small)
            .disabled(model.focusedPhoto.map { $0.isVideo } ?? true)
            .help("Level the horizon Vision finds in the photo (nothing changes when no clear horizon is found)")
        }
    }

    static func help(_ p: CropPreset) -> String {
        switch p {
        case .original: "Back to the whole frame (uncropped)"
        case .free: "Crop to any shape"
        default: "Crop to \(p.rawValue), centred in the current crop"
        }
    }
}

struct SharpeningPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: DS.Space.s) {
            EditSlider(model: model, spec: Specs.sharpenAmount, keyPath: \.detail.sharpenAmount)
            EditSlider(model: model, spec: Specs.sharpenRadius, keyPath: \.detail.sharpenRadius)
            EditSlider(model: model, spec: Specs.sharpenThreshold, keyPath: \.detail.sharpenThreshold)
        }
    }
}

struct NoisePanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: DS.Space.s) {
            AIDenoiseSection(model: model)
            EditSlider(model: model, spec: Specs.noiseLuminance, keyPath: \.noise.luminance)
            EditSlider(model: model, spec: Specs.noiseColor, keyPath: \.noise.color)
            EditSlider(model: model, spec: Specs.noiseDetail, keyPath: \.noise.detail)
        }
    }
}

struct ClarityPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: DS.Space.s) {
            EditSlider(model: model, spec: Specs.clarity, keyPath: \.detail.clarity)
            EditSlider(model: model, spec: Specs.structure, keyPath: \.detail.structure)
        }
    }
}
