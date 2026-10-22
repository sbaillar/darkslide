import DarkslideCore
import SwiftUI

/// Capture One's Copy and Apply: large arrow buttons in the toolbar (or at the foot of the tool
/// panels in the Lightroom Classic layout, named Copy… / Paste there). Copy takes
/// the edited adjustments in one click (its ▾ chooses Edited or Custom); Apply pastes them onto every
/// selected photo. The hover on Apply says what is on the clipboard.
package struct CopyApplyButtons: View {
    @Bindable var model: AppModel
    /// Arrows only on small windows, so the pair fits instead of going to the overflow menu.
    var showsTitles = true
    /// The familiar-layout names (Copy / Apply, or Lightroom's Copy… / Paste).
    var names: NameSet = .darkslide
    /// Centered in a full-width row (the tool panels' foot) rather than sized to fit.
    var centered = false
    @State private var optionsShown = false

    package init(model: AppModel, showsTitles: Bool = true, names: NameSet = .darkslide, centered: Bool = false) {
        self.model = model
        self.showsTitles = showsTitles
        self.names = names
        self.centered = centered
    }

    package var body: some View {
        HStack(spacing: DS.Space.xs) {
            HStack(spacing: 0) {
                Button { model.perform(.copyAdjustments) } label: {
                    Label(names.copyButton, systemImage: "arrow.up")
                }
                .buttonStyle(CopyApplyButtonStyle(edge: .leading, showsTitle: showsTitles))
                .disabled(!CommandCatalog.isEnabled(.copyAdjustments, model: model))
                .help(ToolbarControl.copy.control(model).hint ?? "")
                .accessibilityLabel(ToolbarControl.copy.control(model).label)
                .accessibilityHint(ToolbarControl.copy.control(model).hint ?? "")
                Button { optionsShown.toggle() } label: {
                    Label("Copy options", systemImage: "chevron.down")
                }
                .buttonStyle(CopyApplyButtonStyle(edge: .trailing))
                .help(ToolbarControl.copyOptions.control(model).hint ?? "")
                .accessibilityLabel(ToolbarControl.copyOptions.control(model).label)
                .accessibilityHint(ToolbarControl.copyOptions.control(model).hint ?? "")
                .popover(isPresented: $optionsShown, arrowEdge: .bottom) {
                    CopyOptionsView(options: CopyOptions(app: model))
                }
            }
            if showsTitles, let chip = model.clipboardChip {
                Text(chip)
                    .font(DS.Font.caption.monospacedDigit())
                    .foregroundStyle(DS.textSecondary)
                    .lineLimit(1)
                    .help(model.clipboardSummary)
                    .accessibilityLabel("On the clipboard: \(chip)")
            }
            Button { model.perform(.applyAdjustments) } label: {
                Label(names.applyButton, systemImage: "arrow.down")
            }
            .buttonStyle(CopyApplyButtonStyle(isProminent: true, showsTitle: showsTitles))
            .disabled(!model.canApply)
            .help(model.clipboardSummary)
            .accessibilityLabel(ToolbarControl.apply.control(model).label)
            .accessibilityHint(ToolbarControl.apply.control(model).hint ?? "")
        }
        .fixedSize() // never squeeze "Copy" to "C…"; the toolbar overflows other items first
        .frame(maxWidth: centered ? .infinity : nil)
    }
}

/// The large Copy/Apply look: a bold arrow over a capsule, bigger than the other toolbar
/// pills. `edge` joins Copy and its ▾ into one split button; `isProminent` fills Apply with
/// the accent while it can paste.
struct CopyApplyButtonStyle: ButtonStyle {
    var edge: HorizontalEdge?
    var isProminent = false
    var showsTitle = true
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        let chevron = edge == .trailing
        let filled = isProminent && isEnabled
        Group {
            if chevron || !showsTitle {
                configuration.label.labelStyle(.iconOnly)
            } else {
                configuration.label.labelStyle(.titleAndIcon)
            }
        }
            .font(.system(size: chevron ? 10 : 13, weight: .bold))
            .padding(.leading, chevron ? 6 : 12)
            .padding(.trailing, chevron ? 8 : 14)
            .frame(height: 28)
            .foregroundStyle(filled ? Color.black : (isEnabled ? Color.primary : Color.primary.opacity(0.3)))
            .background {
                UnevenRoundedRectangle(cornerRadii: radii, style: .continuous)
                    .fill(filled ? DS.accent.opacity(configuration.isPressed ? 0.75 : 1)
                        : Color.primary.opacity(!isEnabled ? 0 : (configuration.isPressed ? 0.2 : 0.1)))
            }
            .overlay {
                if !isEnabled {
                    UnevenRoundedRectangle(cornerRadii: radii, style: .continuous)
                        .strokeBorder(Color.primary.opacity(0.15), style: StrokeStyle(lineWidth: 1, dash: [2, 2]))
                }
            }
            .overlay(alignment: .leading) { // the split between Copy and its ▾
                if chevron { Rectangle().fill(Color.primary.opacity(0.2)).frame(width: 1, height: 16) }
            }
            .contentShape(.rect)
    }

    private var radii: RectangleCornerRadii {
        let r: CGFloat = 14
        switch edge {
        case .leading: return .init(topLeading: r, bottomLeading: r, bottomTrailing: 0, topTrailing: 0)
        case .trailing: return .init(topLeading: 0, bottomLeading: 0, bottomTrailing: r, topTrailing: r)
        case nil: return .init(topLeading: r, bottomLeading: r, bottomTrailing: r, topTrailing: r)
        }
    }
}

/// The ▾ popover: Edited (the source's edited groups, recomputed on each Copy) or Custom
/// ticks, All / None, Absolute or Relative, keeping each photo's straighten, and the
/// clipboard's layers. Changes are kept at once (they are what Apply pastes).
struct CopyOptionsView: View {
    let options: CopyOptions

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Copy").font(.system(size: 13, weight: .semibold))
                Text(subtitle).font(DS.Font.label).foregroundStyle(DS.textSecondary)
            }
            .padding(DS.Space.m)
            Divider()
            Picker("Copy", selection: Binding(get: { options.mode }, set: { m in
                if m == .edited { options.selectEdited() } else { options.selectCustom() }
            })) {
                Text("Edited").tag(CopyMode.edited)
                Text("Custom").tag(CopyMode.custom)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.horizontal, DS.Space.m)
            .padding(.top, DS.Space.s)
            .help("Edited copies what was changed on the photo (an unedited photo copies nothing); Custom copies your ticks")
            HStack(spacing: DS.Space.s) {
                Text("ADJUSTMENTS").font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
                Spacer()
                Button("All", action: options.selectAll).help("Tick every group (Custom)")
                Button("None", action: options.selectNone).help("Untick every group (Custom)")
            }
            .buttonStyle(.borderless)
            .font(DS.Font.label)
            .padding(.horizontal, DS.Space.m)
            .padding(.vertical, DS.Space.s)
            if let only = options.onlyGroup {
                Label("Apply pastes only \(only.title) (copied from its panel) until you copy again or change these options.",
                      systemImage: "info.circle")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                    .padding(.horizontal, DS.Space.m)
            }
            ScrollView {
                VStack(spacing: 0) {
                    ForEach(AdjustGroup.allCases) { g in
                        CopyGroupRow(group: g, options: options)
                        if g == .crop {
                            Toggle("Keep each photo's straighten", isOn: Binding(get: { options.keepStraighten },
                                                                                 set: { _ in options.toggleKeepStraighten() }))
                                .toggleStyle(.checkbox)
                                .font(.system(size: 11))
                                .padding(.leading, 34)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .disabled(!options.groups.contains(.crop))
                                .help("Paste the crop but leave every photo's own horizon (and rotation) as it is")
                        }
                    }
                    if !options.layers.isEmpty {
                        layerList
                    }
                }
                .padding(.horizontal, DS.Space.s)
            }
            .frame(maxHeight: 480)
            Divider()
            Label("Relative adds the change (+0.3 EV stays +0.3 EV). Crops adapt to portrait and landscape.",
                  systemImage: "plusminus")
                .font(DS.Font.caption)
                .foregroundStyle(DS.textSecondary)
                .padding(DS.Space.m)
        }
        .frame(width: 380)
    }

    private var subtitle: String {
        guard let name = options.sourceName else { return "Choose what Copy takes and Apply pastes." }
        return options.app.clipboard == nil
            ? "Copy will take these from \(name)." : "On the clipboard: \(name). Apply pastes what is ticked."
    }

    private var layerList: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            Text("LAYERS").font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
            ForEach(options.layers) { layer in
                Toggle(layer.name, isOn: Binding(get: { options.isLayerOn(layer.id) }, set: { _ in options.toggleLayer(layer.id) }))
                    .toggleStyle(.checkbox)
            }
        }
        .font(.system(size: 12))
        .disabled(!options.groups.contains(.layers))
        .padding(DS.Space.s)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct CopyGroupRow: View {
    let group: AdjustGroup
    let options: CopyOptions

    var body: some View {
        HStack(spacing: DS.Space.s) {
            Toggle(isOn: Binding(get: { options.groups.contains(group) }, set: { _ in options.toggle(group) })) {
                HStack(spacing: DS.Space.s) {
                    Image(systemName: group.symbol).frame(width: 18).foregroundStyle(DS.textSecondary)
                    Text(group.title)
                    if options.changedGroups.contains(group) {
                        Circle().fill(DS.accent).frame(width: 5, height: 5).help("Edited on the source photo")
                    }
                }
            }
            .toggleStyle(.checkbox)
            Spacer()
            if group.supportsRelative {
                Picker("\(group.title) paste mode", selection: Binding(get: { options.relative.contains(group) },
                                                                     set: { if $0 != options.relative.contains(group) { options.toggleRelative(group) } })) {
                    Text("Absolute").tag(false)
                    Text("Relative").tag(true)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .controlSize(.mini)
                .frame(width: 120)
                .disabled(!options.groups.contains(group))
                .help("Relative adds the change instead of replacing the value")
            }
        }
        .font(.system(size: 12))
        .padding(.horizontal, DS.Space.s)
        .frame(height: 26)
    }
}
