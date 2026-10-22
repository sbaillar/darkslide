import SwiftUI

/// The body of one tool panel.
struct ToolPanelContent: View {
    let panel: ToolPanelID
    @Bindable var model: AppModel

    var body: some View {
        if panel.group != nil || panel == .metadata || panel == .keywords || panel == .history, model.focusedID == nil {
            PanelEmptyState(symbol: "photo", text: "Select an image to adjust it.")
        } else {
            content
        }
    }

    @ViewBuilder private var content: some View {
        switch panel {
        case .smartFolders: SmartFoldersPanel(model: model)
        case .sessions: SessionsPanel(model: model)
        case .albums: AlbumsPanel(model: model)
        case .filters: FiltersPanel(model: model)
        case .importCard: MemoryCardPanel(model: model)
        case .archive: ArchivePanel(model: model)
        case .styles: StylesPanel(model: model)
        case .whiteBalance: WhiteBalancePanel(model: model)
        case .colorBalance: ColorBalancePanel(model: model)
        case .colorEditor: ColorEditorPanel(model: model)
        case .exposure: ExposurePanel(model: model)
        case .hdr: HDRPanel(model: model)
        case .levels: LevelsPanel(model: model)
        case .curve: CurvePanel(model: model)
        case .lensCorrection: LensPanel(model: model)
        case .vignette: VignettePanel(model: model)
        case .effects: EffectsPanel(model: model)
        case .crop: CropPanel(model: model)
        case .sharpening: SharpeningPanel(model: model)
        case .noise: NoisePanel(model: model)
        case .clarity: ClarityPanel(model: model)
        case .layers: LayersPanel(model: model)
        case .presence: PresencePanel(model: model)
        case .spots: SpotsPanel(model: model)
        case .metadata: MetadataPanel(model: model)
        case .keywords: KeywordsPanel(model: model)
        case .export: ExportPanel(model: model)
        case .amazonPhotos: AmazonPhotosPanel(settings: model.settings)
        case .people: PeoplePanel(model: model, people: model.people)
        case .history: HistoryPanel(model: model)
        }
    }
}

/// A slider bound to one `Double` of the focused edit. Each drag is one undo step
/// (the coalesce key is the slider label).
struct EditSlider: View {
    @Bindable var model: AppModel
    let spec: SliderSpec
    let keyPath: WritableKeyPath<EditState, Double>
    var gradient: Gradient?
    /// Extra mutation applied with every change (e.g. leave as-shot WB).
    var also: ((inout EditState) -> Void)?
    /// Shown instead of the stored value (e.g. the real as-shot Kelvin while As Shot).
    var shownValue: Double?

    var body: some View {
        let value = shownValue ?? model.focusedID.map { model.edit(for: $0)[keyPath: keyPath] } ?? spec.defaultValue
        let field = AIFieldMap.field(for: keyPath)
        ToolSlider(spec: spec, value: value, trackGradient: gradient, onEnd: { model.endEditGesture() },
                   aiNote: field.flatMap(model.aiNote), aiUndo: field.flatMap(model.aiUndo)) { v in
            model.updateEdit(label: spec.label, coalesceKey: "\(spec.label)#\(keyPath.hashValue)") { e in
                e[keyPath: keyPath] = v
                also?(&e)
            }
        }
    }
}

/// Ends the model's edit gesture on mouse-up anywhere in a control (curves,
/// wheels, levels), so each drag is its own undo step.
struct EndsEditGesture: ViewModifier {
    let model: AppModel

    func body(content: Content) -> some View {
        content.simultaneousGesture(DragGesture(minimumDistance: 0).onEnded { _ in model.endEditGesture() })
    }
}

extension View {
    func endsEditGesture(_ model: AppModel) -> some View { modifier(EndsEditGesture(model: model)) }
}

/// The focused image's edit (identity when nothing is focused).
extension AppModel {
    var focusedEdit: EditState { focusedID.map { edit(for: $0) } ?? EditState() }
}

struct PanelEmptyState: View {
    let symbol: String
    let text: String

    var body: some View {
        HStack(spacing: DS.Space.s) {
            Image(systemName: symbol).foregroundStyle(DS.textTertiary)
            Text(text).font(DS.Font.label).foregroundStyle(DS.textSecondary)
            Spacer(minLength: 0)
        }
        .padding(.vertical, DS.Space.xs)
    }
}

/// Small explanatory footnote under a panel.
struct PanelNote: View {
    let text: String
    var body: some View {
        Text(text)
            .font(DS.Font.caption)
            .foregroundStyle(DS.textSecondary)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Label/value row for read-only info.
struct InfoRow: View {
    let label: String
    let value: String
    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            Text(label).font(DS.Font.label).foregroundStyle(DS.textSecondary).frame(width: 72, alignment: .leading)
            Text(value).font(DS.Font.value).lineLimit(1).truncationMode(.middle)
            Spacer(minLength: 0)
        }
    }
}

/// Compact segmented-style chip.
struct Chip: View {
    let title: String
    var color: Color?
    let selected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: DS.Space.xs) {
                if let color { Circle().fill(color).frame(width: 7, height: 7) }
                Text(title).font(DS.Font.caption).lineLimit(1)
            }
            .padding(.horizontal, DS.Space.s)
            .padding(.vertical, DS.Space.xs)
            .background(selected ? DS.accent.opacity(0.22) : Color.primary.opacity(0.06), in: .capsule)
            .overlay(Capsule().strokeBorder(selected ? DS.accent.opacity(0.7) : .clear))
        }
        .buttonStyle(.plain)
        .accessibilityLabel(title)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
