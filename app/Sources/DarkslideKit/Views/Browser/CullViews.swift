import SwiftUI

/// N-up survey (⌥⌘S): the candidates side by side, eliminate with X.
struct SurveyView: View {
    @Bindable var model: AppModel
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        let survey = model.survey
        let ids = survey.visible
        let cols = SurveyModel.columns(for: ids.count)
        VStack(spacing: DS.Space.m) {
            HStack {
                Label("Survey · \(survey.countLabel)", systemImage: "square.grid.2x2")
                    .font(DS.Font.title)
                if survey.pageCount > 1 {
                    Button { survey.turnPage(by: -1); followFocus() } label: { Image(systemName: "chevron.left") }
                        .disabled(survey.page == 0)
                        .help("Previous page ([)")
                    Text("Page \(survey.page + 1) of \(survey.pageCount)").font(DS.Font.label).monospacedDigit()
                    Button { survey.turnPage(by: 1); followFocus() } label: { Image(systemName: "chevron.right") }
                        .disabled(survey.page == survey.pageCount - 1)
                        .help("Next page (])")
                }
                Spacer()
                Text("X eliminate · U put back · ←/→ move · 1–5 rate · P pick · ⏎ keep (pick) · Esc leave")
                    .font(DS.Font.label)
                    .foregroundStyle(DS.textSecondary)
            }
            GeometryReader { geo in
                let rows = max(1, Int(ceil(Double(ids.count) / Double(cols))))
                let spacing = DS.Space.s
                let w = (geo.size.width - spacing * CGFloat(cols - 1)) / CGFloat(cols)
                let h = (geo.size.height - spacing * CGFloat(rows - 1)) / CGFloat(rows)
                LazyVGrid(columns: Array(repeating: GridItem(.fixed(w), spacing: spacing), count: cols), spacing: spacing) {
                    ForEach(ids, id: \.self) { id in
                        if let item = model.item(id) {
                            SurveyCell(item: item, size: CGSize(width: w, height: h), cache: model.thumbnails,
                                       isFocused: survey.focused == id,
                                       onFocus: {
                                           survey.focus(id)
                                           model.focus(id, extend: false)
                                       },
                                       onEliminate: { survey.eliminate(id) })
                        }
                    }
                }
            }
        }
        .padding(DS.Space.l)
        .background(DS.viewerBackground)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Survey of \(survey.candidates.count) photos")
    }

    private func followFocus() {
        if let f = model.survey.focused { model.focus(f, extend: false) }
    }
}

private struct SurveyCell: View {
    let item: PhotoItem
    let size: CGSize
    let cache: ThumbnailCache
    let isFocused: Bool
    let onFocus: () -> Void
    let onEliminate: () -> Void

    var body: some View {
        ZStack(alignment: .topTrailing) {
            // Focus is a ring only: the grid's accent fill tints a tile this big brown in light mode.
            ThumbnailCell(item: item, size: size, style: .grid, isSelected: false, isFocused: false, cache: cache)
                .overlay {
                    if isFocused {
                        RoundedRectangle(cornerRadius: DS.Radius.s).strokeBorder(DS.accent, lineWidth: 2.5)
                    }
                }
                .accessibilityAddTraits(isFocused ? .isSelected : [])
                .onTapGesture(perform: onFocus)
            Button(action: onEliminate) {
                Image(systemName: "xmark.circle.fill").font(.system(size: 18))
                    .foregroundStyle(.white, .black.opacity(0.6))
            }
            .buttonStyle(.plain)
            .padding(DS.Space.s)
            .help("Eliminate (X)")
            .accessibilityLabel("Eliminate \(item.photo.filename)")
        }
    }
}

/// The cull-finishing commands in the browser's context menu (grid and filmstrip).
struct CullContextMenu: View {
    let model: AppModel

    var body: some View {
        ForEach(Array(CommandCatalog.cullContextMenu.enumerated()), id: \.offset) { _, c in
            if let c {
                Button(CommandCatalog.title(c, model: model)) { CommandCatalog.perform(c, model: model) }
                    .disabled(!CommandCatalog.isEnabled(c, model: model))
                    .help(CommandCatalog.help(c, model: model) ?? "")
            } else {
                Divider()
            }
        }
    }
}

/// "Move Picked and Rated to Selects…" and "Delete Rejected…" confirmations.
struct CullDialogs: ViewModifier {
    @Bindable var model: AppModel

    func body(content: Content) -> some View {
        let cull = model.cull
        content.confirmationDialog(
            "Move to Selects?",
            isPresented: Binding(get: { cull.pendingSelects != nil }, set: { if !$0 { cull.pendingSelects = nil } }),
            titleVisibility: .visible
        ) {
            let n = cull.pendingSelects?.ids.count ?? 0
            Button("Move \(n) \(n == 1 ? "Photo" : "Photos")") { Task { await cull.confirmMoveToSelects() } }
            Button("Cancel", role: .cancel) { cull.pendingSelects = nil }
        } message: {
            Text((cull.pendingSelects?.message ?? "") + ". Change the rating in Settings.")
        }
        .confirmationDialog(
            "Delete Rejected Files?",
            isPresented: Binding(get: { cull.pendingDelete != nil }, set: { if !$0 { cull.pendingDelete = nil } }),
            titleVisibility: .visible
        ) {
            Button("Move to Trash", role: .destructive) { Task { await cull.confirmDeleteRejected() } }
            Button("Cancel", role: .cancel) { cull.pendingDelete = nil }
        } message: {
            Text(CullFinisher.deleteMessage(cull.pendingDelete ?? 0, archiveNote: cull.deleteArchiveNote))
        }
    }
}
