import AppKit
import DarkslideCore
import DarkslideFFI
import DarkslideUI
import SwiftUI

/// [library] The Library mode: Sessions, Dates and Collections on the left, a
/// filter bar and a virtualized grid of every session's photos. Logic lives in
/// `LibraryModel`; the grid is rows of a range (no per-photo identity to diff),
/// and each cell reads its details and thumbnail from the model's caches.
struct LibraryBrowser: View {
    @Bindable var model: AppModel
    @Bindable var library: LibraryModel

    var body: some View {
        HStack(spacing: 0) {
            LibrarySidebar(library: library)
                .frame(width: 230)
            Divider()
            VStack(spacing: 0) {
                LibraryFilterBar(library: library)
                Divider()
                LibraryGrid(library: library)
            }
        }
        .background(DS.viewerBackground)
        .onAppear { library.appeared() }
    }
}

// MARK: - Sidebar

private struct LibrarySidebar: View {
    @Bindable var library: LibraryModel
    @State private var naming: Naming?

    enum Naming: Identifiable {
        case collection, smart, rename(Int64)
        var id: String {
            switch self {
            case .collection: "c"
            case .smart: "s"
            case .rename(let id): "r\(id)"
            }
        }
    }

    var body: some View {
        let o = library.overview
        List(selection: Binding(get: { library.scope }, set: { if let s = $0 { library.scope = s } })) {
            Label("All Photographs", systemImage: "photo.on.rectangle")
                .badge(Int(o?.total ?? 0))
                .tag(LibraryModel.Scope.all)
            Section("Collections") {
                ForEach(library.collections, id: \.id) { c in
                    Label(c.name, systemImage: c.rules == nil ? "rectangle.stack" : "gearshape.2")
                        .badge(Int(c.count))
                        .tag(LibraryModel.Scope.collection(c.id))
                        .contextMenu {
                            if c.rules == nil {
                                Button("Add Selected Photos") { Task { await library.addSelection(to: c.id) } }
                                    .disabled(library.selection.isEmpty)
                            }
                            Button("Rename…") { naming = .rename(c.id) }
                            Button("Delete") { Task { await library.deleteCollection(c.id) } }
                        }
                }
                Menu("New Collection") {
                    Button("From Selection…") { naming = .collection }
                    Button("Smart Collection from Filter…") { naming = .smart }
                }
                .menuStyle(.borderlessButton)
                .font(DS.Font.label)
            }
            Section("Sessions") {
                ForEach(o?.sessions ?? [], id: \.id) { s in
                    Label(s.name, systemImage: s.available ? "folder" : "externaldrive.badge.xmark")
                        .foregroundStyle(s.available ? .primary : DS.textSecondary)
                        .badge(Int(s.count))
                        .help(s.available ? s.root : "\(s.root) is offline: its photos are listed, not shown")
                        .tag(LibraryModel.Scope.session(s.id))
                }
            }
            Section("Dates") {
                ForEach(LibraryDates.tree(o?.days ?? []), id: \.year) { y in
                    DisclosureGroup {
                        ForEach(y.months, id: \.month) { m in
                            DisclosureGroup {
                                ForEach(m.days, id: \.day) { d in
                                    Text("\(d.day)").badge(Int(d.count))
                                        .tag(LibraryModel.Scope.day(y.year, m.month, d.day))
                                }
                            } label: {
                                Text(LibraryDates.monthName(m.month)).badge(Int(m.count))
                                    .tag(LibraryModel.Scope.month(y.year, m.month))
                            }
                        }
                    } label: {
                        Text(String(y.year)).badge(Int(y.count))
                            .tag(LibraryModel.Scope.year(y.year))
                    }
                }
            }
        }
        .listStyle(.sidebar)
        .sheet(item: $naming) { n in
            NameSheet(title: title(n), initial: initial(n)) { name in
                naming = nil
                guard let name else { return }
                Task {
                    switch n {
                    case .collection: await library.newCollection(named: name)
                    case .smart: await library.newSmartCollection(named: name)
                    case .rename(let id): await library.renameCollection(id, to: name)
                    }
                }
            }
        }
    }

    private func title(_ n: Naming) -> String {
        switch n {
        case .collection: "New Collection from \(library.selection.count) Selected"
        case .smart: "New Smart Collection from the Current Filter"
        case .rename: "Rename Collection"
        }
    }

    private func initial(_ n: Naming) -> String {
        if case .rename(let id) = n { return library.collections.first { $0.id == id }?.name ?? "" }
        return ""
    }
}

private struct NameSheet: View {
    let title: String
    let initial: String
    let done: (String?) -> Void
    @State private var name = ""

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text(title).font(.headline)
            TextField("Name", text: $name).textFieldStyle(.roundedBorder).frame(width: 300)
            HStack {
                Spacer()
                Button("Cancel") { done(nil) }.keyboardShortcut(.cancelAction)
                Button("OK") { done(name) }.keyboardShortcut(.defaultAction)
                    .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(DS.Space.l)
        .onAppear { name = initial }
    }
}

// MARK: - Filter bar

private struct LibraryFilterBar: View {
    @Bindable var library: LibraryModel

    var body: some View {
        let o = library.overview
        HStack(spacing: DS.Space.m) {
            Text(library.title).font(.system(size: 13, weight: .semibold)).lineLimit(1)
            Picker("Rating", selection: $library.filter.minRating) {
                Text("Any rating").tag(0)
                ForEach(1...5, id: \.self) { n in Text("≥ \(String(repeating: "★", count: n))").tag(n) }
            }
            .labelsHidden().fixedSize().help("Minimum rating")
            Picker("Flag", selection: $library.filter.pick) {
                ForEach(PhotoFilter.barPickOptions, id: \.self) { Text($0.rawValue) }
            }
            .pickerStyle(.segmented).labelsHidden().fixedSize().help("Filter by pick flag")
            HStack(spacing: 3) {
                ForEach(ColorTag.allCases.filter { $0 != .none }, id: \.self) { tag in
                    Button {
                        library.filter.label = library.filter.label == tag ? nil : tag
                    } label: {
                        Circle().fill(DS.tagColor(tag)).frame(width: 11, height: 11)
                            .overlay(Circle().strokeBorder(library.filter.label == tag ? Color.primary : .black.opacity(0.4),
                                                           lineWidth: library.filter.label == tag ? 2 : 1))
                            .padding(2).contentShape(.circle)
                    }
                    .buttonStyle(.plain)
                    .help("\(tag.name) label")
                    .accessibilityLabel("\(tag.name) label filter")
                    .accessibilityAddTraits(library.filter.label == tag ? .isSelected : [])
                }
            }
            Picker("Camera", selection: $library.filter.camera) {
                Text("Any camera").tag(String?.none)
                ForEach(o?.cameras ?? [], id: \.name) { Text($0.name).tag(Optional($0.name)) }
            }
            .labelsHidden().fixedSize().help("Camera")
            Picker("Keyword", selection: $library.filter.keyword) {
                Text("Any keyword").tag(String?.none)
                ForEach(o?.keywords ?? [], id: \.name) { Text($0.name).tag(Optional($0.name)) }
            }
            .labelsHidden().fixedSize().help("Keyword")
            TextField("Find names, titles, keywords", text: $library.filter.text)
                .textFieldStyle(.roundedBorder)
                .frame(maxWidth: 220)
            Spacer(minLength: 0)
            Text(library.countText).font(DS.Font.value).foregroundStyle(DS.textSecondary)
            if library.filter.isActive {
                Button("Clear") { library.filter = LibraryFilter() }
            }
            Picker("Sort", selection: $library.filter.sort) {
                Text("Capture Time").tag(LibrarySortKey.captured)
                Text("Rating").tag(LibrarySortKey.rating)
                Text("File Name").tag(LibrarySortKey.name)
                Text("Session").tag(LibrarySortKey.session)
            }
            .labelsHidden().fixedSize().help("Sort")
            Button {
                library.filter.descending.toggle()
            } label: {
                Image(systemName: library.filter.descending ? "arrow.down" : "arrow.up")
            }
            .help(library.filter.descending ? "Descending" : "Ascending")
            .accessibilityLabel("Sort direction")
        }
        .controlSize(.small)
        .padding(.horizontal, DS.Space.m)
        .frame(height: 36)
        .background(DS.chrome)
    }
}

// MARK: - Grid

private struct LibraryGrid: View {
    @Bindable var library: LibraryModel
    @State private var cellSize: CGFloat = 180
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        if library.count == 0 {
            VStack(spacing: DS.Space.s) {
                switch library.phase {
                case .loading: ProgressView()
                case .failed(let why): Text(why).foregroundStyle(DS.reject)
                case .idle:
                    Text(library.overview?.total == 0 ? "No photos yet: open or import a session and it appears here"
                        : "No photos match")
                        .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            GeometryReader { geo in
                let spacing = DS.Space.s
                let cols = max(1, Int((geo.size.width - 2 * DS.Space.m + spacing) / (cellSize + spacing)))
                let cell = CGSize(width: cellSize, height: (cellSize * 0.8).rounded() + 20)
                let rows = (library.count + cols - 1) / cols
                let bucket = ThumbnailLoader.bucket(points: cellSize, scale: displayScale)
                ScrollView {
                    LazyVStack(spacing: spacing) {
                        ForEach(0..<rows, id: \.self) { r in
                            HStack(spacing: spacing) {
                                ForEach(r * cols..<min(library.count, (r + 1) * cols), id: \.self) { i in
                                    LibraryCell(library: library, index: i, size: cell, bucket: bucket)
                                }
                                Spacer(minLength: 0)
                            }
                        }
                    }
                    .padding(DS.Space.m)
                }
                .onAppear { library.columns = cols }
                .onChange(of: cols) { _, c in library.columns = c }
            }
            .overlay(alignment: .bottomTrailing) {
                Slider(value: $cellSize, in: 110...320)
                    .frame(width: 120).controlSize(.small)
                    .padding(DS.Space.s)
                    .background(DS.chrome.opacity(0.9), in: .capsule)
                    .padding(DS.Space.m)
                    .help("Thumbnail size")
            }
        }
    }
}

/// One photo, styled like the browser grid's cell. Reads only its own details.
private struct LibraryCell: View {
    let library: LibraryModel
    let index: Int
    let size: CGSize
    let bucket: Int
    @State private var image: CGImage?
    @State private var failed = false

    private var box: CGSize { CGSize(width: size.width, height: size.height - 20) }

    var body: some View {
        let _ = library.itemsVersion // details patched in place
        let item = library.item(at: index)
        let selected = item.map { library.selection.contains($0.id) } ?? false
        let focused = item != nil && item?.id == library.focused
        return content(item)
            .frame(width: size.width, height: size.height)
            .background(background(selected: selected, focused: focused), in: .rect(cornerRadius: DS.Radius.s))
            .overlay { border(selected: selected, focused: focused) }
            .contentShape(.rect)
            .help(item.map { "\($0.sessionName) · \(Self.fileName($0))" } ?? "")
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(item.map { LibraryText.label($0) } ?? "Loading")
            .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
            .onTapGesture { click(item) }
            .simultaneousGesture(TapGesture(count: 2).onEnded { open(item) })
            .contextMenu { if let item { LibraryMenu(library: library, item: item) } }
            .onAppear { library.cellAppeared(index, bucket: bucket) }
            .task(id: LibraryText.thumbKey(item, bucket)) { await load(item) }
    }

    private static func fileName(_ i: LibraryItemRecord) -> String { URL(fileURLWithPath: i.path).lastPathComponent }

    private func content(_ item: LibraryItemRecord?) -> some View {
        let aspect = image.map { Double($0.width) / Double(max($0.height, 1)) } ?? 1.5
        let rect = ThumbnailLayout.fit(aspect: aspect, in: box, inset: DS.Space.s)
        return VStack(spacing: 0) {
            ZStack(alignment: .topLeading) {
                picture(item)
                    .frame(width: rect.width, height: rect.height)
                    .offset(x: rect.minX, y: rect.minY)
                if let item { badges(item, rect) }
            }
            .frame(width: box.width, height: box.height, alignment: .topLeading)
            caption(item)
        }
    }

    @ViewBuilder private func picture(_ item: LibraryItemRecord?) -> some View {
        if let image {
            let rejected = item?.pick == .rejected
            Image(decorative: image, scale: 1).resizable().interpolation(.high)
                .clipShape(.rect(cornerRadius: 2))
                .shadow(color: .black.opacity(0.4), radius: 2, y: 1)
                .saturation(rejected ? 0.15 : 1)
                .opacity(rejected ? 0.35 : 1)
        } else {
            RoundedRectangle(cornerRadius: 2).fill(Color.primary.opacity(0.07))
                .overlay {
                    if failed {
                        Image(systemName: "externaldrive.badge.xmark").foregroundStyle(DS.textSecondary)
                            .help("The original isn’t reachable (offline or moved)")
                    }
                }
        }
    }

    private func caption(_ item: LibraryItemRecord?) -> some View {
        HStack(spacing: DS.Space.xs) {
            Text(item.map(Self.fileName) ?? " ")
                .font(.system(size: 11)).foregroundStyle(.primary.opacity(0.85))
                .lineLimit(1).truncationMode(.middle)
            Spacer(minLength: 0)
            StarsView(rating: Int(item?.rating ?? 0), size: 10)
        }
        .padding(.horizontal, DS.Space.s)
        .frame(height: 20)
    }

    private func background(selected: Bool, focused: Bool) -> Color {
        if focused { return DS.accent.opacity(0.20) }
        if selected { return DS.accent.opacity(0.10) }
        return DS.cellBackground.opacity(0.55)
    }

    private func border(selected: Bool, focused: Bool) -> some View {
        let color: Color = focused ? DS.accent : (selected ? DS.accent.opacity(0.55) : DS.hairline)
        return RoundedRectangle(cornerRadius: DS.Radius.s).strokeBorder(color, lineWidth: focused ? 2.5 : 1)
    }

    private func click(_ item: LibraryItemRecord?) {
        guard let id = item?.id else { return }
        let mods = NSEvent.modifierFlags
        library.click(id, command: mods.contains(.command), shift: mods.contains(.shift))
    }

    private func open(_ item: LibraryItemRecord?) {
        guard let id = item?.id else { return }
        Task { await library.open(id) }
    }

    private func load(_ item: LibraryItemRecord?) async {
        guard let item else { return }
        if let hit = library.thumbnails.cached(item, bucket: bucket) {
            image = hit
            return
        }
        failed = false
        let img = await library.thumbnails.image(for: item, bucket: bucket)
        guard !Task.isCancelled else { return }
        image = img
        failed = img == nil
    }

    private func badges(_ item: LibraryItemRecord, _ rect: CGRect) -> some View {
        let inner = rect.insetBy(dx: 4, dy: 4)
        return ZStack(alignment: .topLeading) {
            switch item.pick {
            case .picked:
                Image(systemName: "flag.fill").font(.system(size: 11)).foregroundStyle(DS.pick)
                    .padding(3).background(.black.opacity(0.55), in: .circle)
                    .position(x: inner.minX + 8, y: inner.minY + 8)
            case .rejected:
                Image(systemName: "xmark").font(.system(size: 10, weight: .heavy)).foregroundStyle(.white)
                    .padding(4).background(DS.reject, in: .circle)
                    .position(x: inner.minX + 8, y: inner.minY + 8)
            case .unflagged: EmptyView()
            }
            if item.editHash != nil {
                Image(systemName: "slider.horizontal.3").font(.system(size: 9, weight: .bold))
                    .padding(3).background(.black.opacity(0.6), in: .rect(cornerRadius: 3)).foregroundStyle(.white)
                    .position(x: inner.maxX - 9, y: inner.minY + 9)
            }
            if item.label != .none {
                TagChip(tag: RustSessionStore.tag(item.label))
                    .position(x: inner.maxX - 9, y: inner.maxY - 9)
            }
        }
        .allowsHitTesting(false)
    }
}

private struct LibraryMenu: View {
    let library: LibraryModel
    let item: LibraryItemRecord

    var body: some View {
        Button("Open in Session") { Task { await library.open(item.id) } }
        Divider()
        Menu("Rating") {
            ForEach(0...5, id: \.self) { n in
                Button(n == 0 ? "No Rating" : String(repeating: "★", count: n)) { prepare(); library.rate(n) }
            }
        }
        Menu("Label") {
            ForEach(ColorTag.allCases, id: \.self) { t in Button(t.name) { prepare(); library.setLabel(t) } }
        }
        Menu("Flag") {
            Button("Pick") { prepare(); library.setFlag(.picked) }
            Button("Reject") { prepare(); library.setFlag(.rejected) }
            Button("Unflag") { prepare(); library.setFlag(.none) }
        }
        let plain = library.collections.filter { $0.rules == nil }
        if !plain.isEmpty {
            Menu("Add to Collection") {
                ForEach(plain, id: \.id) { c in
                    Button(c.name) { prepare(); Task { await library.addSelection(to: c.id) } }
                }
            }
        }
        if library.plainCollection != nil {
            Button("Remove from Collection") { prepare(); Task { await library.removeSelectionFromCollection() } }
        }
    }

    /// A right-click on a photo outside the selection acts on that photo.
    private func prepare() {
        if !library.selection.contains(item.id) { library.click(item.id) }
    }
}
