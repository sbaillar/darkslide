import DarkslideCore
import SwiftUI

struct WhiteBalancePanel: View {
    @Bindable var model: AppModel

    private static let kelvinGradient = Gradient(colors: [
        Color(red: 0.35, green: 0.55, blue: 1.0), Color(white: 0.85), Color(red: 1.0, green: 0.7, blue: 0.3),
    ])
    private static let tintGradient = Gradient(colors: [
        Color(red: 0.3, green: 0.85, blue: 0.4), Color(white: 0.85), Color(red: 0.9, green: 0.35, blue: 0.85),
    ])

    var body: some View {
        let wb = model.focusedEdit.whiteBalance
        // [tl8] The camera's real as-shot values (pipeline, live mode).
        let asShot = model.focusedID.flatMap(model.asShot(for:))
        VStack(spacing: DS.Space.s) {
            CameraProfilePicker(model: model)
            HStack(spacing: DS.Space.s) {
                Picker("Mode", selection: Binding(
                    get: { wb.asShot },
                    set: { toAsShot in
                        model.updateEdit(label: toAsShot ? "As Shot WB" : "Custom WB") { e in
                            // Custom starts from what As Shot showed.
                            if !toAsShot, e.whiteBalance.asShot, let a = asShot { e.whiteBalance = .custom(a) }
                            e.whiteBalance.asShot = toAsShot
                        }
                    }
                )) {
                    Text("As Shot").tag(true)
                    Text("Custom").tag(false)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .help("As Shot uses the camera's white balance; Custom uses Kelvin and tint")
                Menu {
                    ForEach(WhiteBalancePreset.allCases) { p in
                        Button {
                            model.updateEdit(label: "WB \(p.rawValue)") { $0.whiteBalance = p.applied(to: $0.whiteBalance) }
                        } label: {
                            Label("\(p.rawValue)  \(Formatters.kelvin(p.temperature))", systemImage: p.symbol)
                        }
                    }
                } label: {
                    Image(systemName: "list.bullet")
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
                .help("White balance presets")
                .accessibilityLabel("White balance presets")
                Button {
                    model.wbPickerActive.toggle()
                } label: {
                    Image(systemName: "eyedropper")
                        .foregroundStyle(model.wbPickerActive ? DS.accent : DS.textSecondary)
                        .frame(width: 22, height: 20)
                        .background(model.wbPickerActive ? DS.accent.opacity(0.2) : .clear, in: .rect(cornerRadius: DS.Radius.s))
                }
                .buttonStyle(.plain)
                .help("White balance picker: click something neutral grey in the image (Esc to cancel)")
                .accessibilityLabel(model.wbPickerActive ? "Cancel white balance picker" : "White balance picker")
            }
            if model.wbPickerActive {
                HStack(spacing: DS.Space.s) {
                    PanelNote(text: "Click a neutral grey or white area. Click again to refine.")
                    Spacer(minLength: 0)
                    Button("Done") { model.wbPickerActive = false }
                        .controlSize(.small)
                        .keyboardShortcut(.defaultAction)
                        .help("Leave the picker (Esc)")
                }
            }
            if model.wbPickerActive || model.colorTools.mode != nil {
                Picker("Sample", selection: Binding(get: { model.settings.pickerSample },
                                                    set: { model.viewer.setPickerSample($0) })) {
                    ForEach(PickerSample.allCases) { Text($0.label).tag($0) }
                }
                .pickerStyle(.menu)
                .controlSize(.small)
                .help("How many pixels the eyedropper averages (the loupe outlines them)")
            }
            EditSlider(model: model, spec: Specs.temperature, keyPath: \.whiteBalance.temperature,
                       gradient: Self.kelvinGradient, also: { e in
                           // Leaving As Shot keeps the as-shot tint (the drag sets the Kelvin).
                           if e.whiteBalance.asShot, let a = asShot { e.whiteBalance.tint = (a.tint * 10).rounded() / 10 }
                           e.whiteBalance.asShot = false
                       }, shownValue: wb.asShot ? asShot.map { $0.kelvin.rounded() } : nil)
            EditSlider(model: model, spec: Specs.tint, keyPath: \.whiteBalance.tint,
                       gradient: Self.tintGradient, also: { e in
                           if e.whiteBalance.asShot, let a = asShot { e.whiteBalance.temperature = a.kelvin.rounded() }
                           e.whiteBalance.asShot = false
                       }, shownValue: wb.asShot ? asShot.map { ($0.tint * 10).rounded() / 10 } : nil)
                .opacity(wb.asShot ? 0.6 : 1)
            if wb.asShot, let a = asShot {
                PanelNote(text: WhiteBalanceReadout.asShot(a))
            }
        }
        .task(id: model.focusedID) { if let id = model.focusedID { await model.loadDevelopInfo(for: [id]) } }
    }
}

struct ColorBalancePanel: View {
    @Bindable var model: AppModel

    var body: some View {
        let cb = model.focusedEdit.colorBalance
        VStack(spacing: DS.Space.m) {
            ColorWheelView(title: "Master", value: cb.master, diameter: 120) { v, key in set(\.master, v, key) }
            HStack(alignment: .top, spacing: DS.Space.s) {
                ColorWheelView(title: "Shadows", value: cb.shadows, diameter: 76) { v, key in set(\.shadows, v, key) }
                ColorWheelView(title: "Midtones", value: cb.midtones, diameter: 76) { v, key in set(\.midtones, v, key) }
                ColorWheelView(title: "Highlights", value: cb.highlights, diameter: 76) { v, key in set(\.highlights, v, key) }
            }
        }
        .frame(maxWidth: .infinity)
        .endsEditGesture(model)
        .aiHighlight(model, AIFieldMap.colorBalance)
    }

    private func set(_ kp: WritableKeyPath<ColorBalanceEdit, WheelValue>, _ v: WheelValue, _ key: String) {
        model.updateEdit(label: "Color Balance", coalesceKey: key) { $0.colorBalance[keyPath: kp] = v }
    }
}

/// Colour editor: 8 basic bands, picked Advanced ranges and skin-tone uniformity, plus
/// the on-image tools (range picker, skin picker, targeted adjustment).
struct ColorEditorPanel: View {
    @Bindable var model: AppModel
    @State private var band = 0
    @State private var mode = ColorEditorMode.basic

    var body: some View {
        let editor = model.focusedEdit.colorEditor
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Picker("Mode", selection: $mode) {
                ForEach(ColorEditorMode.allCases, id: \.self) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            switch mode {
            case .basic: basic(editor)
            case .advanced: advanced(editor)
            case .skin: skin(editor)
            }
        }
        .onDisappear { model.colorTools.cancel() }
    }

    @ViewBuilder private func basic(_ editor: ColorEditorEdit) -> some View {
        // As many columns as fit whole names: four across broke "Magenta" mid-word.
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 78), spacing: DS.Space.xs)], spacing: DS.Space.xs) {
            ForEach(0..<8, id: \.self) { i in
                let c = WheelMath.rgb(hue: ColorEditorEdit.bandHues[i], saturation: 0.8, value: 0.95)
                Chip(title: ColorEditorEdit.bandNames[i], color: Color(red: c.r, green: c.g, blue: c.b),
                     selected: band == i) { band = i }
                    .overlay(alignment: .topTrailing) {
                        if editor.bands[i] != ColorBandEdit() { Circle().fill(DS.accent).frame(width: 5, height: 5) }
                    }
                    .help("Edit \(ColorEditorEdit.bandNames[i].lowercased()) tones")
            }
        }
        EditSlider(model: model, spec: Specs.bandHue, keyPath: \.colorEditor.bands[band].hueShift)
        EditSlider(model: model, spec: Specs.bandSaturation, keyPath: \.colorEditor.bands[band].saturation)
        EditSlider(model: model, spec: Specs.bandLightness, keyPath: \.colorEditor.bands[band].lightness)
        // The caption on its own line and one shared hand icon, so the three names fit
        // whole (side by side they broke as "Saturati / on").
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            Label("Drag on image", systemImage: "hand.point.up.left.and.text")
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            HStack(spacing: DS.Space.xs) {
                ForEach(TargetedChannel.allCases, id: \.self) { ch in
                    toolButton(.targeted(ch), title: ch.rawValue, symbol: nil,
                               help: "Drag up or down on a colour in the image to change its \(ch.rawValue.lowercased())")
                }
            }
        }
        if case .targeted? = model.colorTools.mode {
            PanelNote(text: "Drag up or down on a colour in the image. Esc to stop.")
        }
    }

    @ViewBuilder private func advanced(_ editor: ColorEditorEdit) -> some View {
        HStack {
            toolButton(.pickRange, title: "Pick Color", symbol: "eyedropper.halffull",
                       help: "Click a colour in the image to add a range for it")
            Spacer()
            if let i = selected(editor) {
                Button(role: .destructive) {
                    model.updateEdit(label: "Remove Color Range") { e in
                        e.colorEditor.advanced = ColorRangeOps.removing(i, from: e.colorEditor.advanced)
                    }
                    model.colorTools.selectedRange = ColorRangeOps.selection(afterRemoving: i, count: editor.advanced.count)
                } label: { Image(systemName: "minus.circle") }
                .buttonStyle(.plain)
                .help("Remove this color range")
            }
        }
        if model.colorTools.mode == .pickRange {
            PanelNote(text: "Click the colour you want to change.")
        }
        if editor.advanced.isEmpty {
            PanelNote(text: "Pick a colour in the image to adjust just that colour.")
        } else {
            HStack(spacing: DS.Space.xs) {
                ForEach(editor.advanced.indices, id: \.self) { i in
                    let c = WheelMath.rgb(hue: editor.advanced[i].centerHue, saturation: 0.8, value: 0.95)
                    Chip(title: "\(i + 1)", color: Color(red: c.r, green: c.g, blue: c.b),
                         selected: selected(editor) == i) { model.colorTools.selectedRange = i }
                }
            }
            if let i = selected(editor) {
                rangeSliders(label: "Color Range", get: { $0.colorEditor.advanced[safe: i] },
                             set: { e, r in if i < e.colorEditor.advanced.count { e.colorEditor.advanced[i] = r } })
            }
        }
    }

    @ViewBuilder private func skin(_ editor: ColorEditorEdit) -> some View {
        HStack {
            toolButton(.pickSkin, title: "Pick Skin", symbol: "eyedropper.halffull",
                       help: "Click skin in the image to set the skin-tone range")
            Spacer()
            if editor.skin != nil {
                Button("Reset") { model.updateEdit(label: "Reset Skin Tone") { $0.colorEditor.skin = nil } }
                    .controlSize(.small)
            }
        }
        if model.colorTools.mode == .pickSkin {
            PanelNote(text: "Click an evenly lit area of skin.")
        }
        ClosureSlider(model: model, spec: ColorToolSpecs.hueUniformity, key: "skin.hue",
                      get: { $0.colorEditor.skin?.hueUniformity ?? 0 },
                      set: { e, v in e.colorEditor.skin = (e.colorEditor.skin ?? SkinToneEdit()).with(\.hueUniformity, v) })
        ClosureSlider(model: model, spec: ColorToolSpecs.saturationUniformity, key: "skin.sat",
                      get: { $0.colorEditor.skin?.saturationUniformity ?? 0 },
                      set: { e, v in e.colorEditor.skin = (e.colorEditor.skin ?? SkinToneEdit()).with(\.saturationUniformity, v) })
        ClosureSlider(model: model, spec: ColorToolSpecs.lightnessUniformity, key: "skin.lum",
                      get: { $0.colorEditor.skin?.lightnessUniformity ?? 0 },
                      set: { e, v in e.colorEditor.skin = (e.colorEditor.skin ?? SkinToneEdit()).with(\.lightnessUniformity, v) })
        if editor.skin != nil {
            rangeSliders(label: "Skin Tone", get: { $0.colorEditor.skin?.range },
                         set: { e, r in e.colorEditor.skin?.range = r }, shifts: false)
        }
    }

    /// Centre, width, softness (and shifts) of a range.
    @ViewBuilder private func rangeSliders(label: String, get: @escaping (EditState) -> ColorRangeEdit?,
                                           set: @escaping (inout EditState, ColorRangeEdit) -> Void,
                                           shifts: Bool = true) -> some View {
        let fields: [(SliderSpec, WritableKeyPath<ColorRangeEdit, Double>)] =
            [(ColorToolSpecs.rangeCenter, \.centerHue), (ColorToolSpecs.rangeWidth, \.width),
             (ColorToolSpecs.rangeSmoothness, \.smoothness)]
            + (shifts ? [(Specs.bandHue, \.hueShift), (Specs.bandSaturation, \.saturation), (Specs.bandLightness, \.lightness)] : [])
        ForEach(fields.indices, id: \.self) { k in
            let (spec, kp) = fields[k]
            ClosureSlider(model: model, spec: spec, key: label + spec.label,
                          get: { get($0).map { $0[keyPath: kp] } ?? spec.defaultValue },
                          set: { e, v in if var r = get(e) { r[keyPath: kp] = v; set(&e, r) } })
        }
    }

    private func selected(_ editor: ColorEditorEdit) -> Int? {
        guard let i = model.colorTools.selectedRange, i < editor.advanced.count else {
            return editor.advanced.isEmpty ? nil : editor.advanced.count - 1
        }
        return i
    }

    private func toolButton(_ tool: ColorTool, title: String, symbol: String?, help: String) -> some View {
        let on = model.colorTools.mode == tool
        return Button {
            model.colorTools.mode = on ? nil : tool
        } label: {
            Group {
                if let symbol { Label(title, systemImage: symbol) } else { Text(title) }
            }
                .font(DS.Font.caption)
                .lineLimit(1)
                .padding(.horizontal, DS.Space.xs)
                .padding(.vertical, 2)
                .background(on ? DS.accent.opacity(0.2) : .clear, in: .rect(cornerRadius: DS.Radius.s))
                .foregroundStyle(on ? DS.accent : DS.text)
        }
        .buttonStyle(.plain)
        .help(help)
        .accessibilityLabel(on ? "Stop \(title.lowercased())" : title)
    }
}

enum ColorEditorMode: String, CaseIterable {
    case basic = "Basic", advanced = "Advanced", skin = "Skin Tone"
}

/// A slider over any value of the edit (the value may not exist yet: `get` supplies
/// what to show, `set` creates it).
struct ClosureSlider: View {
    @Bindable var model: AppModel
    let spec: SliderSpec
    let key: String
    let get: (EditState) -> Double
    let set: (inout EditState, Double) -> Void

    var body: some View {
        let value = get(model.focusedEdit)
        ToolSlider(spec: spec, value: value, onEnd: { model.endEditGesture() }) { v in
            model.updateEdit(label: spec.label, coalesceKey: key) { e in set(&e, v) }
        }
    }
}
