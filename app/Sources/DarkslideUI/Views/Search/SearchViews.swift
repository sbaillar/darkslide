import DarkslideCore
import SwiftUI

/// The natural-language search field in the filter bar: "kids on the beach at
/// sunset". Searches as you type (this session, or all sessions), offers the
/// one-time model download, Find Similar and keyword suggestions.
struct PhotoSearchField: View {
    @Bindable var model: AppModel

    private var search: PhotoSearch { model.search }

    var body: some View {
        @Bindable var search = model.search
        HStack(spacing: DS.Space.xs) {
            Image(systemName: search.similarTo == nil ? "sparkle.magnifyingglass" : "square.on.square")
                .foregroundStyle(search.isActive ? DS.accent : DS.textSecondary)
            TextField(search.similarTo == nil ? "Search photos — “red car”" : "Similar photos", text: $search.query)
                .textFieldStyle(.plain)
                .frame(width: 190)
                .onSubmit { search.submit() }
                .onChange(of: search.query) { _, q in
                    // Find Similar empties the field itself: keep its results.
                    if q.isEmpty { if search.similarTo == nil { search.clear() } } else { search.queryChanged() }
                }
                .accessibilityLabel("Search photos by description")
            if search.isActive || !search.query.isEmpty {
                Button {
                    search.query = ""
                    search.clear()
                } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(DS.textSecondary)
                }
                .buttonStyle(.plain)
                .help("Show all photos")
                .accessibilityLabel("Clear search")
            }
            status
            menu
        }
        .padding(.horizontal, DS.Space.xs)
        .padding(.vertical, 2)
        .background(RoundedRectangle(cornerRadius: DS.Radius.s).fill(Color.primary.opacity(0.06)))
        .popover(isPresented: Binding(get: { search.keywordSuggestions != nil },
                                     set: { if !$0 { search.dismissKeywordSuggestions() } }),
                      arrowEdge: .bottom) { keywordList }
    }

    @ViewBuilder private var status: some View {
        switch search.models?.state {
        case .downloading(let f)?:
            ProgressView(value: f).frame(width: 60)
            Button("Cancel") { search.cancelDownload() }.buttonStyle(.borderless)
        default:
            switch search.phase {
            case .needsModel:
                Button("Download search model (\(ByteCountFormatter.string(fromByteCount: search.models?.descriptor.totalBytes ?? 0, countStyle: .file)))") {
                    Task { await search.downloadModel() }
                }
                .buttonStyle(.borderless)
                .help("MobileCLIP runs on this Mac; photos never leave it")
            case .searching:
                ProgressView().controlSize(.mini)
            case .failed(let m):
                Image(systemName: "exclamationmark.triangle").foregroundStyle(DS.warningText).help(m)
            case .idle, .results:
                EmptyView()
            }
        }
    }

    private var menu: some View {
        Menu {
            Picker("Search In", selection: Bindable(search).scope) {
                ForEach(PhotoSearch.Scope.allCases, id: \.self) { Text($0.rawValue) }
            }
            .onChange(of: search.scope) { _, _ in search.submit() }
            Divider()
            Button("Find Similar Photos") {
                if let id = model.focusedID { Task { await search.findSimilar(id) } }
            }
            .disabled(model.focusedID == nil || !search.isAvailable)
            Button("Suggest Keywords…") {
                guard let id = model.focusedID else { return }
                Task { await search.showKeywordSuggestions(for: id) }
            }
            .disabled(model.focusedID == nil || !(search.models?.isReady ?? false))
        } label: {
            Image(systemName: "chevron.down")
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Search scope, Find Similar, keyword suggestions")
    }

    private var keywordList: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            Text("Suggested keywords").font(DS.Font.label).foregroundStyle(DS.textSecondary)
            let keywords = search.keywordSuggestions?.words ?? []
            if keywords.isEmpty {
                Text("None yet: the photo isn't indexed for search.").font(DS.Font.label)
            }
            ForEach(keywords, id: \.self) { k in
                Button {
                    search.addSuggestedKeyword(k)
                } label: {
                    Label(k, systemImage: "plus.circle")
                }
                .buttonStyle(.borderless)
            }
        }
        .padding(DS.Space.s)
    }
}

/// "Indexing for search… 120 of 2,000" in the status bar.
package struct SearchStatusLabel: View {
    let search: PhotoSearch

    package init(search: PhotoSearch) { self.search = search }

    package var body: some View {
        if let m = search.indexingMessage {
            Label(m, systemImage: "sparkle.magnifyingglass")
                .foregroundStyle(DS.textSecondary)
                .accessibilityLabel(m)
        }
    }
}

/// "Search all sessions": results grouped by session, each with Open Session.
package struct SessionResultsPanel: View {
    @Bindable var model: AppModel

    package init(model: AppModel) {
        self.model = model
    }

    package var body: some View {
        let search = model.search
        if !search.sessionResults.isEmpty {
            VStack(alignment: .leading, spacing: DS.Space.s) {
                HStack {
                    Text("“\(search.query)” in all sessions").font(.headline)
                    Spacer()
                    Button {
                        search.clear()
                    } label: {
                        Image(systemName: "xmark")
                    }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Close results")
                }
                ScrollView {
                    VStack(alignment: .leading, spacing: DS.Space.m) {
                        ForEach(search.sessionResults) { group in
                            SessionResultRow(model: model, group: group)
                        }
                    }
                }
            }
            .padding(DS.Space.m)
            .frame(width: 420)
            .frame(maxHeight: 520)
            .background(RoundedRectangle(cornerRadius: DS.Radius.l).fill(.regularMaterial))
            .padding(DS.Space.m)
        }
    }
}

private struct SessionResultRow: View {
    let model: AppModel
    let group: SessionMatches

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            HStack {
                Text(group.name).fontWeight(.semibold)
                Text("\(group.hits.count) photo\(group.hits.count == 1 ? "" : "s")").foregroundStyle(DS.textSecondary)
                if !group.available { Text("offline").foregroundStyle(DS.warningText) }
                Spacer()
                Button("Open Session") { Task { await model.search.open(group) } }
                    .disabled(!group.available)
            }
            .font(DS.Font.label)
            ScrollView(.horizontal) {
                HStack(spacing: DS.Space.xs) {
                    ForEach(group.hits.prefix(12), id: \.url) { hit in
                        SearchHitThumb(model: model, url: hit.url)
                            .onTapGesture { Task { await model.search.open(group, photo: hit.url) } }
                            .help(hit.url.lastPathComponent)
                            .accessibilityElement(children: .ignore)
                            .accessibilityLabel(hit.url.lastPathComponent)
                            .accessibilityAddTraits(.isButton)
                            .accessibilityAction { Task { await model.search.open(group, photo: hit.url) } }
                    }
                }
            }
        }
    }
}

private struct SearchHitThumb: View {
    let model: AppModel
    let url: URL
    @State private var image: CGImage?

    var body: some View {
        Group {
            if let image {
                Image(decorative: image, scale: 1).resizable().scaledToFill()
            } else {
                Color.primary.opacity(0.08)
            }
        }
        .frame(width: 72, height: 72)
        .clipShape(RoundedRectangle(cornerRadius: DS.Radius.s))
        .task(id: url) { image = try? await model.previews?.preview(for: url, maxPixelSize: 160) }
    }
}
