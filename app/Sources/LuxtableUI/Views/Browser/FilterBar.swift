import LuxtableCore
import SwiftUI

/// Filter/sort bar above the browser: stars ≥, Unrated, pick state, color tags,
/// search, sort, "n of m", Clear. All mutations go through `PhotoFilter` helpers.
struct FilterBar: View {
    @Bindable var model: AppModel
    /// Compact variant for the filmstrip (no search field, smaller spacing).
    let compact: Bool

    private var controls: [FilterBarControl] {
        FilterBarControl.all(filter: model.filter, missingCount: model.missingCount, compact: compact)
    }

    private func a11y(_ c: FilterBarControl) -> AccessibleControl {
        c.control(filter: model.filter, missingCount: model.missingCount, sort: model.sort)
    }

    var body: some View {
        HStack(spacing: compact ? DS.Space.s : DS.Space.m) {
            // [multiview] First, so a narrow column beside the viewer still shows it.
            if compact && model.browserMode == .filmstrip {
                FilmstripSizeSlider(model: model)
                Toggle("Stack bursts", isOn: Binding(get: { model.browserState.stackBursts }, set: { model.setStackBursts($0) }))
                    .toggleStyle(.checkbox)
                    .fixedSize()
                    .help("Show each burst as one stack; click a stack's badge to see its frames and choose its cover")
                Divider().frame(height: 14)
            }
            stars
            chip(.unrated, "Unrated", on: model.filter.unratedOnly)

            Picker("Flag", selection: $model.filter.pick) {
                ForEach(PhotoFilter.barPickOptions, id: \.self) { Text($0.rawValue) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            .help("Filter by pick flag")
            .accessibilityLabel("Pick flag filter")

            HStack(spacing: 3) {
                ForEach(ColorTag.allCases.filter { $0 != .none }, id: \.self) { tag in
                    let c = a11y(.tag(tag))
                    Button {
                        FilterBarControl.tag(tag).apply(to: &model.filter)
                    } label: {
                        Circle().fill(DS.tagColor(tag)).frame(width: 11, height: 11)
                            .overlay(Circle().strokeBorder(model.filter.colorTag == tag ? Color.primary : .black.opacity(0.4),
                                                           lineWidth: model.filter.colorTag == tag ? 2 : 1))
                            .padding(2)
                            .contentShape(.circle)
                    }
                    .buttonStyle(.plain)
                    .help(c.hint ?? c.label)
                    .accessibilityLabel(c.label)
                    .accessibilityAddTraits(model.filter.colorTag == tag ? .isSelected : [])
                }
            }

            if controls.contains(.missing) {
                chip(.missing, MissingActions.filterTitle(count: model.missingCount), on: model.filter.missingOnly)
            }
            if model.filter.missingOnly {
                MissingActionsBar(model: model)
            }
            if !MissingActions.changed(model).isEmpty {
                FileChangedActionsBar(model: model)
            }

            if !compact {
                if model.search.isAvailable { PhotoSearchField(model: model) } // [search]
                TextField("Search names, titles", text: $model.filter.text)
                    .textFieldStyle(.roundedBorder)
                    .frame(maxWidth: 200)
                    .accessibilityLabel(a11y(.search).label)
            }
            if let place = model.filter.place { // [map] a marker's photos or an area
                Button(place.title, systemImage: "xmark.circle.fill") { model.map.clearFilter() }
                    .buttonStyle(FilterChipStyle(on: true))
                    .help("Show the photos everywhere again")
            }
            Spacer(minLength: 0)
            // One line or nothing: in a narrow window the count wrapped to two lines. The
            // status bar shows it too, so it is the first thing to give up its room.
            ViewThatFits(in: .horizontal) {
                Text(FilterSummary.text(shown: model.visibleIDs.count, total: model.photoCount,
                                        filtered: model.filter.isActive))
                    .font(DS.Font.value)
                    .foregroundStyle(DS.textSecondary)
                    .lineLimit(1)
                    .fixedSize()
                Color.clear.frame(width: 0, height: 0)
            }
            if model.filter.isActive {
                Button("Clear (\(model.filter.activeCriteria))") { FilterBarControl.clear.apply(to: &model.filter) }
                    .buttonStyle(.borderless)
                    .help("Show all photos")
                    .accessibilityLabel(a11y(.clear).label)
            }
            SortMenu(model: model)
                .accessibilityLabel(a11y(.sort).label)
        }
        .font(DS.Font.label)
        .controlSize(.small)
        .padding(.horizontal, DS.Space.m)
        .frame(height: compact ? 28 : 34)
        .background(DS.chrome)
    }

    private func chip(_ c: FilterBarControl, _ title: String, on: Bool) -> some View {
        let info = a11y(c)
        return Button {
            c.apply(to: &model.filter)
        } label: {
            Text(title).lineLimit(1).fixedSize() // never "U…"
        }
        .buttonStyle(FilterChipStyle(on: on))
        .help(info.hint ?? info.label)
        .accessibilityLabel(info.label)
        .accessibilityAddTraits(on ? .isSelected : [])
    }

    private var stars: some View {
        HStack(spacing: 1) {
            ForEach(1...5, id: \.self) { n in
                let info = a11y(.star(n))
                Button {
                    FilterBarControl.star(n).apply(to: &model.filter)
                } label: {
                    Image(systemName: n <= model.filter.minRating ? "star.fill" : "star")
                        .font(.system(size: 11))
                        .foregroundStyle(n <= model.filter.minRating ? DS.star : DS.textSecondary)
                        .padding(1)
                        .contentShape(.rect)
                }
                .buttonStyle(.plain)
                .help(info.hint ?? info.label)
                .accessibilityLabel(info.label)
                .accessibilityAddTraits(model.filter.minRating == n ? .isSelected : [])
            }
        }
    }
}

/// [multiview] The filmstrip's thumbnail size: small and large photo icons either side
/// (View ▸ Larger / Smaller Filmstrip Thumbnails step it too).
struct FilmstripSizeSlider: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: DS.Space.xs) {
            Image(systemName: "photo").font(.system(size: 8)).foregroundStyle(DS.textSecondary)
            Slider(value: Binding(get: { model.filmstrip.cell }, set: { model.sizeFilmstripThumbnails($0) }),
                   in: FilmstripSizing.cellHeights)
                .frame(width: 80)
            Image(systemName: "photo").font(.system(size: 13)).foregroundStyle(DS.textSecondary)
        }
        .help("Thumbnail size (⌃⌘= larger, ⌃⌘- smaller)")
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Filmstrip thumbnail size")
        .accessibilityValue("\(Int(model.filmstrip.cell)) points")
        .accessibilityAdjustableAction { dir in
            CommandCatalog.perform(dir == .increment ? .largerFilmstripThumbnails : .smallerFilmstripThumbnails, model: model)
        }
    }
}

/// Remove / Locate for the Missing filter.
struct MissingActionsBar: View {
    @Bindable var model: AppModel

    var body: some View {
        let removable = MissingActions.removable(model)
        let c = MissingActions.controls(selected: removable.count)
        HStack(spacing: DS.Space.xs) {
            Button("Remove…") {
                guard ConfirmRemove.run(count: removable.count) else { return }
                Task { await model.removeMissing(removable) }
            }
                .disabled(removable.isEmpty)
                .help(c[0].hint ?? c[0].label)
                .accessibilityLabel(c[0].label)
            Button("Locate…") { locate() }
                .disabled(model.focusedPhoto?.isMissing != true)
                .help(c[1].hint ?? c[1].label)
                .accessibilityLabel(c[1].label)
            let restorable = MissingActions.restorable(model)
            if !restorable.isEmpty {
                let r = MissingActions.restoreToFolder
                Button(r.label) { Task { await model.restoreToFolder(restorable) } }
                    .help(r.hint ?? r.label)
                    .accessibilityLabel(r.label)
            }
        }
        .buttonStyle(.bordered)
    }

    private func locate() {
        guard let id = model.focusedID, let name = model.focusedPhoto?.filename else { return }
        guard let url = PlatformPanels.chooseItem(message: "Where is \(name) now?") else { return }
        Task { await model.locate(id, at: url) }
    }
}

/// Restore Previous / Keep as New for "File changed" photos (R4-H3).
struct FileChangedActionsBar: View {
    @Bindable var model: AppModel

    var body: some View {
        let ids = MissingActions.changed(model)
        HStack(spacing: DS.Space.xs) {
            ForEach([MissingActions.restorePrevious, MissingActions.keepAsNew], id: \.id) { c in
                Button(c.label) {
                    Task {
                        if c.id == MissingActions.restorePrevious.id {
                            await model.restorePrevious(ids)
                        } else {
                            await model.keepAsNew(ids)
                        }
                    }
                }
                .help(c.hint ?? c.label)
                .accessibilityLabel(c.label)
            }
        }
        .buttonStyle(.bordered)
    }
}

/// Sort order menu. Bound to the browser's sort order.
struct SortMenu: View {
    @Bindable var model: AppModel

    var body: some View {
        Menu {
            Picker("Sort by", selection: $model.sort) {
                ForEach(PhotoSort.allCases, id: \.self) { s in
                    Label(s.rawValue, systemImage: s.symbol).tag(s)
                }
            }
            .pickerStyle(.inline)
        } label: {
            Label(model.sort.rawValue, systemImage: "arrow.up.arrow.down")
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .help("Sort order")
    }
}

/// Toggle-look chip for filter buttons.
struct FilterChipStyle: ButtonStyle {
    let on: Bool

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 11, weight: on ? .semibold : .regular))
            .padding(.horizontal, 8).padding(.vertical, 2)
            .foregroundStyle(on ? Color.black : .primary)
            .background(on ? DS.accent : Color.primary.opacity(configuration.isPressed ? 0.16 : 0.08), in: .capsule)
    }
}
