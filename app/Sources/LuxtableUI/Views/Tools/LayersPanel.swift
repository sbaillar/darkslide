import LuxtableCore
import SwiftUI

/// Layer list (add, duplicate, rename, delete, show/hide, opacity), the selected layer's
/// tools (exposure, tone, white balance, presence, HDR, detail), mask drawing tools with
/// a brush strip, editable gradients, mask refinement (feather, luminance and colour
/// ranges) and on-device AI masks (Subject, Person, Background, Sky). All logic is in
/// `LayerController` (one selection per photo; changes hit the focused photo only).
struct LayersPanel: View {
    @Bindable var model: AppModel
    @State private var busy: AITarget?
    @State private var error: String?

    private var layers: LayerController { LayerController(app: model) }

    var body: some View {
        let list = layers.layers
        let selected = layers.selected
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack(spacing: DS.Space.s) {
                Text("\(list.count) layer\(list.count == 1 ? "" : "s") over Background")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .aiHighlight(model, AIFieldMap.layers, value: "")
                Spacer()
                Button { layers.add() } label: { Image(systemName: "plus") }
                    .help("Add an adjustment layer")
                    .accessibilityLabel("Add layer")
                Button { layers.duplicateSelected() } label: { Image(systemName: "plus.square.on.square") }
                    .disabled(selected == nil)
                    .help("Duplicate the selected layer")
                    .accessibilityLabel("Duplicate layer")
                Button { layers.deleteSelected() } label: { Image(systemName: "minus") }
                    .disabled(selected == nil)
                    .help("Delete the selected layer")
                    .accessibilityLabel("Delete layer")
            }
            .buttonStyle(.borderless)
            if list.isEmpty {
                PanelNote(text: "Layers apply local adjustments through a mask. Add one, or start with an AI mask below.")
            }
            ForEach(list.reversed()) { layer in
                LayerRow(layer: layer, selected: selected == layer.id, model: model, controller: layers)
            }
            Text("DRAW MASK").font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
                .padding(.top, DS.Space.xs)
            HStack(spacing: DS.Space.xs) {
                maskTool("paintbrush.pointed", "Brush", .brush)
                maskTool("eraser", "Erase", .eraser)
                maskTool("rectangle.lefthalf.inset.filled", "Linear", .linearGradient)
                maskTool("circle.dashed", "Radial", .radialGradient)
            }
            if [.brush, .eraser].contains(model.viewer.canvasTool) {
                BrushStrip(viewer: model.viewer)
            } else if [.linearGradient, .radialGradient].contains(model.viewer.canvasTool) {
                PanelNote(text: "Drag on the image to add a gradient; drag its handles to change it.")
            }
            Text("AI MASK (ON-DEVICE)").font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
                .padding(.top, DS.Space.xs)
            // Two by two: four across cut "Background" to "Backg…" in the sidebar.
            Grid(horizontalSpacing: DS.Space.xs, verticalSpacing: DS.Space.xs) {
                GridRow {
                    aiButton(.subject, "person.and.background.dotted")
                    aiButton(.person, "person.fill")
                }
                GridRow {
                    aiButton(.background, "photo")
                    aiButton(.sky, "cloud.sun")
                }
            }
            PeopleMaskMenu(model: model, controller: layers)
            if case .downloading(let f) = SkySegmentation.shared.state {
                HStack(alignment: .bottom, spacing: DS.Space.s) {
                    ProgressView(value: f) { Text("Downloading the sky model (DETR-ResNet50, Apple)…").font(DS.Font.caption) }
                        .controlSize(.small)
                    Button("Cancel") { SkySegmentation.shared.cancel() }
                        .controlSize(.small)
                        .help("Stop downloading the sky model")
                        .accessibilityLabel("Cancel the sky model download")
                }
            }
            if let error {
                Label(error, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.reject)
            }
        }
    }

    /// Canvas tools act on the selected layer; the viewer draws the strokes/gradients.
    private func maskTool(_ symbol: String, _ title: String, _ tool: CanvasTool) -> some View {
        Button {
            layers.chooseTool(tool)
        } label: {
            Label(title, systemImage: symbol).labelStyle(.iconOnly).frame(maxWidth: .infinity)
                .foregroundStyle(model.viewer.canvasTool == tool ? DS.accent : DS.text)
        }
        .controlSize(.small)
        .help("\(title) mask on the selected layer: draw on the image")
        .accessibilityLabel("\(title) mask tool")
        .accessibilityAddTraits(model.viewer.canvasTool == tool ? .isSelected : [])
    }

    private func aiButton(_ target: AITarget, _ symbol: String) -> some View {
        Button {
            Task { await runAI(target) }
        } label: {
            HStack(spacing: 3) {
                if busy == target { ProgressView().controlSize(.mini) } else { Image(systemName: symbol) }
                Text(target.rawValue.capitalized).font(DS.Font.caption)
            }
            .frame(maxWidth: .infinity)
        }
        .controlSize(.small)
        .disabled(!AIMasker.isAvailable(target) || busy != nil || model.focusedID == nil)
        .help(AIMasker.help(target))
        .accessibilityLabel("AI \(target.rawValue) mask")
    }

    private func runAI(_ target: AITarget) async {
        busy = target
        error = nil
        defer { busy = nil }
        do {
            try await layers.addAIMask(target)
        } catch is CancellationError {
            // The sky model download was cancelled: nothing to report.
        } catch {
            self.error = error.localizedDescription
        }
    }
}

private struct LayerRow: View {
    let layer: LayerEdit
    let selected: Bool
    @Bindable var model: AppModel
    let controller: LayerController
    @State private var renaming = false
    @State private var name = ""
    @FocusState private var nameFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            HStack {
                Button {
                    controller.update(layer.id, label: "Toggle Layer") { $0.enabled.toggle() }
                } label: {
                    Image(systemName: layer.enabled ? "eye" : "eye.slash")
                        .foregroundStyle(layer.enabled ? DS.text : DS.textTertiary)
                }
                .buttonStyle(.plain)
                .help(layer.enabled ? "Hide this layer" : "Show this layer")
                .accessibilityLabel(layer.enabled ? "Hide \(layer.name)" : "Show \(layer.name)")
                if renaming {
                    TextField("Name", text: $name)
                        .textFieldStyle(.plain)
                        .font(DS.Font.label)
                        .focused($nameFocused)
                        .onSubmit(commitRename)
                        .onExitCommand { renaming = false }
                        .onAppear { nameFocused = true }
                } else {
                    // A real button: keyboard and VoiceOver can select the row (N17).
                    Button {
                        controller.select(layer.id)
                    } label: {
                        HStack {
                            Text(layer.name).font(DS.Font.label).foregroundStyle(DS.text)
                            Spacer()
                            Text("\(Safe.clampedInt((layer.opacity * 100))) %").font(DS.Font.value).foregroundStyle(DS.textSecondary)
                        }
                        .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                    .simultaneousGesture(TapGesture(count: 2).onEnded {
                        name = layer.name
                        renaming = true
                    })
                    .help("Select this layer. Double-click to rename.")
                    .accessibilityLabel(LayerController.accessibilityLabel(layer, selected: selected))
                    .accessibilityAction(named: "Rename") {
                        name = layer.name
                        renaming = true
                    }
                }
            }
            Text(LayerOps.maskSummary(layer.mask)).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            if selected {
                ToolSlider(spec: Specs.layerOpacity, value: layer.opacity, onEnd: { model.endEditGesture() }) { v in
                    controller.update(layer.id, label: "Layer Opacity", key: "opacity-\(layer.id)") { $0.opacity = v }
                }
                ToolSlider(spec: Specs.layerExposure, value: layer.exposureEV, onEnd: { model.endEditGesture() }) { v in
                    controller.update(layer.id, label: "Layer Exposure", key: "exposure-\(layer.id)") { $0.exposureEV = v }
                }
                LayerToolsSection(layer: layer, model: model, controller: controller)
                LayerMaskSection(layer: layer, model: model, controller: controller)
            }
        }
        .padding(DS.Space.s)
        .background(selected ? DS.accent.opacity(0.12) : Color.primary.opacity(0.04), in: .rect(cornerRadius: DS.Radius.m))
        .overlay(RoundedRectangle(cornerRadius: DS.Radius.m).strokeBorder(selected ? DS.accent.opacity(0.5) : .clear))
        .accessibilityElement(children: .contain)
    }

    private func commitRename() {
        controller.rename(layer.id, to: name)
        renaming = false
    }
}

/// The selected layer's tools beyond exposure (lt-core `LayerAdjustments`).
private struct LayerToolsSection: View {
    let layer: LayerEdit
    @Bindable var model: AppModel
    let controller: LayerController
    @State private var open: Set<String> = ["Tone"]

    var body: some View {
        let t = layer.tools
        group("Tone") {
            slider(Specs.contrast, t.contrast, "contrast") { $0.contrast = $1 }
            slider(Specs.brightness, t.brightness, "brightness") { $0.brightness = $1 }
            slider(Specs.saturation, t.saturation, "saturation") { $0.saturation = $1 }
        }
        group("White Balance") {
            slider(LayerToolSpecs.temperatureShift, t.temperature, "temp") { $0.temperature = $1 }
            slider(LayerToolSpecs.tintShift, t.tint, "tint") { $0.tint = $1 }
        }
        group("Presence") {
            slider(LayerToolSpecs.vibrance, t.presence.vibrance, "vibrance") { $0.presence.vibrance = $1 }
            slider(LayerToolSpecs.texture, t.presence.texture, "texture") { $0.presence.texture = $1 }
            slider(LayerToolSpecs.dehaze, t.presence.dehaze, "dehaze") { $0.presence.dehaze = $1 }
        }
        group("High Dynamic Range") {
            slider(Specs.highlights, t.hdr.highlights, "highlights") { $0.hdr.highlights = $1 }
            slider(Specs.shadows, t.hdr.shadows, "shadows") { $0.hdr.shadows = $1 }
            slider(Specs.whites, t.hdr.whites, "whites") { $0.hdr.whites = $1 }
            slider(Specs.blacks, t.hdr.blacks, "blacks") { $0.hdr.blacks = $1 }
        }
        group("Detail") {
            slider(Specs.clarity, t.clarity, "clarity") { $0.clarity = $1 }
            slider(Specs.structure, t.structure, "structure") { $0.structure = $1 }
            slider(LayerToolSpecs.sharpening, t.sharpening, "sharpening") { $0.sharpening = $1 }
        }
    }

    private func group<Content: View>(_ title: String, @ViewBuilder _ content: @escaping () -> Content) -> some View {
        DisclosureGroup(isExpanded: Binding(get: { open.contains(title) },
                                            set: { if $0 { open.insert(title) } else { open.remove(title) } })) {
            VStack(spacing: DS.Space.xs) { content() }
        } label: {
            Text(title.uppercased()).font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
        }
    }

    private func slider(_ spec: SliderSpec, _ value: Double, _ key: String,
                        _ set: @escaping (inout LayerToolsEdit, Double) -> Void) -> some View {
        ToolSlider(spec: spec, value: value, onEnd: { model.endEditGesture() }) { v in
            controller.updateTools(label: "Layer \(spec.label)", key: key) { set(&$0, v) }
        }
    }
}

/// Mask refinement: invert, feather, luminance range and a picked colour range.
private struct LayerMaskSection: View {
    let layer: LayerEdit
    @Bindable var model: AppModel
    let controller: LayerController

    var body: some View {
        let m = layer.mask
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            Text("REFINE MASK").font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
            Toggle("Invert mask", isOn: Binding(get: { m.invert },
                                                set: { v in controller.update(layer.id, label: "Invert Mask") { $0.mask.invert = v } }))
                .font(DS.Font.label)
                .disabled(m.shapes.isEmpty)
            ToolSlider(spec: LayerToolSpecs.maskFeather, value: m.feather, onEnd: { model.endEditGesture() }) { v in
                controller.update(layer.id, label: "Mask Feather", key: "feather-\(layer.id)") { $0.mask.feather = v }
            }
            Toggle("Luminance range", isOn: Binding(get: { m.lumaRange != nil }, set: { controller.setLumaRange($0) }))
                .font(DS.Font.label)
            if let r = m.lumaRange {
                luma(LayerToolSpecs.lumaLow, r.low, "low") { $0.low = $1 }
                luma(LayerToolSpecs.lumaHigh, r.high, "high") { $0.high = $1 }
                luma(LayerToolSpecs.lumaFalloff, r.lowFalloff, "falloff") { $0.lowFalloff = $1; $0.highFalloff = $1 }
            }
            HStack {
                Text("Color range").font(DS.Font.label)
                if let c = m.colorRange {
                    let rgb = WheelMath.rgb(hue: c.centerHue, saturation: 0.8, value: 0.95)
                    Circle().fill(Color(red: rgb.r, green: rgb.g, blue: rgb.b)).frame(width: 10, height: 10)
                }
                Spacer()
                Button {
                    model.colorTools.mode = model.colorTools.mode == .pickMaskRange ? nil : .pickMaskRange
                } label: {
                    Image(systemName: "eyedropper.halffull")
                        .foregroundStyle(model.colorTools.mode == .pickMaskRange ? DS.accent : DS.text)
                }
                .buttonStyle(.plain)
                .help("Pick a colour in the image: the layer applies only to that colour")
                .accessibilityLabel("Pick mask colour range")
                if m.colorRange != nil {
                    Button { controller.clearColorRange() } label: { Image(systemName: "xmark.circle") }
                        .buttonStyle(.plain)
                        .help("Remove the colour range")
                        .accessibilityLabel("Remove mask colour range")
                }
            }
            if let c = m.colorRange {
                ToolSlider(spec: ColorToolSpecs.rangeWidth, value: c.width, onEnd: { model.endEditGesture() }) { v in
                    controller.update(layer.id, label: "Range Width", key: "crw-\(layer.id)") { $0.mask.colorRange?.width = v }
                }
                ToolSlider(spec: ColorToolSpecs.rangeSmoothness, value: c.smoothness, onEnd: { model.endEditGesture() }) { v in
                    controller.update(layer.id, label: "Range Softness", key: "crs-\(layer.id)") { $0.mask.colorRange?.smoothness = v }
                }
            }
        }
    }

    private func luma(_ spec: SliderSpec, _ value: Double, _ key: String,
                      _ set: @escaping (inout LumaRangeEdit, Double) -> Void) -> some View {
        ToolSlider(spec: spec, value: value, onEnd: { model.endEditGesture() }) { v in
            controller.update(layer.id, label: "Luma Range", key: "luma-\(key)-\(layer.id)") { l in
                if var r = l.mask.lumaRange {
                    set(&r, v)
                    l.mask.lumaRange = r
                }
            }
        }
    }
}

/// Brush size ([ and ]), hardness, flow and edge-aware auto-mask for new strokes.
private struct BrushStrip: View {
    @Bindable var viewer: ViewerModel

    var body: some View {
        VStack(spacing: DS.Space.xs) {
            ToolSlider(spec: LayerToolSpecs.brushSize, value: viewer.brushSize) { viewer.brushSize = $0 }
            ToolSlider(spec: LayerToolSpecs.brushHardness, value: viewer.brushHardness) { viewer.brushHardness = $0 }
            ToolSlider(spec: LayerToolSpecs.brushFlow, value: viewer.brushFlow) { viewer.brushFlow = $0 }
            Toggle("Auto mask (follow edges)", isOn: $viewer.brushAutoMask)
                .font(DS.Font.label)
                .help("Keep brush strokes inside the edges they start in")
        }
    }
}
