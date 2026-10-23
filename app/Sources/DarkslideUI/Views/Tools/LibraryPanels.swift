import DarkslideCore
import SwiftUI

struct SessionsPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xxs) {
            if model.sessionList.isEmpty {
                PanelEmptyState(symbol: "folder", text: "No sessions yet. Import a card to create one.")
            }
            ForEach(model.sessionList) { s in
                let current = model.currentSession?.id == s.id
                Button {
                    LocatePanel.openRecent(s, model: model)
                } label: {
                    HStack(spacing: DS.Space.s) {
                        Image(systemName: current ? "folder.fill" : s.unavailable == nil ? "folder" : "questionmark.folder")
                            .foregroundStyle(current ? DS.accent : DS.textSecondary)
                        VStack(alignment: .leading, spacing: 1) {
                            Text(s.name).font(DS.Font.label).lineLimit(1)
                            Text(s.unavailable?.label ?? AppModel.sessionSubtitle(s)).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                                .lineLimit(1).truncationMode(.middle)
                        }
                        Spacer()
                    }
                    .padding(.horizontal, DS.Space.s)
                    .padding(.vertical, DS.Space.xs)
                    .background(current ? DS.accent.opacity(0.12) : .clear, in: .rect(cornerRadius: DS.Radius.m))
                    .contentShape(.rect)
                }
                .buttonStyle(.plain)
                .opacity(s.unavailable == nil ? 1 : 0.5)
                .help(LocatePanel.help(s))
                .contextMenu {
                    Button("Show in Finder") { PlatformWorkspace.reveal([s.url]) }
                        .disabled(s.unavailable?.folderMissing == true)
                    Button("Locate Folder…") {
                        guard let url = LocatePanel.folder(for: s) else { return }
                        Task { await model.locateSession(s, at: url) }
                    }
                    Button("Remove from List") { Task { await model.removeFromList(s) } }
                        .disabled(!model.canRemoveFromList(s))
                }
                .accessibilityLabel("Session \(s.name), \(s.unavailable?.label ?? PanelChrome.photoCount(s.photoCount))")
                .accessibilityAddTraits(current ? .isSelected : [])
            }
        }
    }
}

struct AlbumsPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            if model.albums.isEmpty {
                PanelEmptyState(symbol: "rectangle.stack", text: "No albums in this session.")
            }
            ForEach(model.albums) { a in
                HStack(spacing: DS.Space.s) {
                    Image(systemName: "rectangle.stack").foregroundStyle(DS.textSecondary)
                    Text(a.name).font(DS.Font.label)
                    Spacer()
                    Text("\(a.photoIDs.count)").font(DS.Font.value).foregroundStyle(DS.textSecondary)
                }
                .padding(.horizontal, DS.Space.s)
                .help("Album “\(a.name)”: use it as a Copy/Apply target")
            }
        }
    }
}

struct FiltersPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            TextField("Search names, titles, captions", text: $model.filter.text)
                .textFieldStyle(.roundedBorder)
                .controlSize(.small)
                .help("Show only photos whose file name, title or caption contains this text")
            // Rating, flag and colour filters live once, in the filter bar over the
            // browser (final review: the same controls were here twice).
            PanelNote(text: "Filter by rating, flag and colour in the bar above the thumbnails.")
            if model.filter.isActive {
                Button("Clear Filters") { model.clearFilters() }
                    .controlSize(.small)
                    .help("Clear the search and filters (a selected smart folder stays selected)")
            }
        }
    }
}

struct MemoryCardPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            PanelNote(text: ImportWording.cardPanelNote(verifies: model.importer is any ImportVerifying, live: model.mode == .live))
            Button {
                model.sheet = .importCard(nil)
            } label: {
                Label(model.mode == .live ? "Browse a Card…" : "Import from Card…", systemImage: "sdcard")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .tint(DS.accent)
            .help("Open the import sheet (⇧⌘I)")
        }
    }
}

struct ArchivePanel: View {
    @Bindable var model: AppModel

    var body: some View {
        let s = model.archiveStatus
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack(spacing: DS.Space.s) {
                Image(systemName: ArchiveDisplay.symbol(s.state)).foregroundStyle(color(s.state))
                Text(ArchiveDisplay.title(s.state)).font(DS.Font.label)
                Spacer()
            }
            if let note = ArchiveDisplay.exclusionNote(s, settings: model.settings.archiveSnapshot) {
                Text(note).font(DS.Font.caption).foregroundStyle(DS.textSecondary) // N2
            }
            if case .syncing(let f) = s.state {
                ProgressView(value: f).controlSize(.small)
            }
            InfoRow(label: "Target", value: s.target)
            InfoRow(label: "Last sync", value: Formatters.relative(s.lastSuccess))
            Button {
                Task { await model.archive.syncNow() }
            } label: {
                Label("Sync Now", systemImage: "arrow.triangle.2.circlepath")
            }
            .controlSize(.small)
            .help("Copy new and changed files to the archive now (never deletes)")
        }
    }

    private func color(_ s: ArchiveStatus.State) -> Color {
        switch s {
        case .idle: DS.pick
        case .syncing: DS.accent
        case .behind, .notArchived: DS.star
        case .offline, .failed, .problems: DS.reject
        case .unavailable: DS.textSecondary
        }
    }
}

struct MetadataPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        if let p = model.focusedPhoto {
            VStack(alignment: .leading, spacing: DS.Space.xs) {
                InfoRow(label: "File", value: p.filename)
                InfoRow(label: "Captured", value: p.meta.captureDate.formatted(date: .abbreviated, time: .standard))
                InfoRow(label: "Camera", value: p.meta.camera)
                InfoRow(label: "Lens", value: p.meta.lens)
                InfoRow(label: "Exposure", value: "\(Formatters.shutter(p.meta.shutter))  \(Formatters.aperture(p.meta.aperture))  ISO \(p.meta.iso)")
                InfoRow(label: "Focal", value: Formatters.focal(p.meta.focalLength))
                InfoRow(label: "Size", value: "\(p.meta.pixelWidth) × \(p.meta.pixelHeight) · \(Formatters.bytes(p.meta.fileSize))")
                Divider().padding(.vertical, DS.Space.xs)
                HStack(spacing: DS.Space.xs) {
                    Text("Rating").font(DS.Font.label).foregroundStyle(DS.textSecondary).frame(width: 72, alignment: .leading)
                    ForEach(1...5, id: \.self) { r in
                        Button { model.setRating(p.rating == r ? 0 : r) } label: {
                            Image(systemName: r <= p.rating ? "star.fill" : "star")
                                .font(.system(size: 11))
                                .foregroundStyle(r <= p.rating ? DS.star : DS.textSecondary)
                        }
                        .buttonStyle(.plain)
                        .help("Rate \(r) (key \(r))")
                        .accessibilityLabel("Rate \(r) star\(r == 1 ? "" : "s")")
                        .accessibilityAddTraits(r == p.rating ? .isSelected : [])
                    }
                }
                HStack(spacing: DS.Space.xs) {
                    Text("Flag").font(DS.Font.label).foregroundStyle(DS.textSecondary).frame(width: 72, alignment: .leading)
                    Chip(title: "Pick", color: DS.pick, selected: p.pick == .picked) {
                        model.setPick(p.pick == .picked ? .none : .picked)
                    }.help("Pick (P)")
                    Chip(title: "Reject", color: DS.reject, selected: p.pick == .rejected) {
                        model.setPick(p.pick == .rejected ? .none : .rejected)
                    }.help("Reject (X)")
                }
                if let note = AICullApplication.rejectNote(p) {
                    HStack(alignment: .firstTextBaseline, spacing: DS.Space.xs) {
                        Text("AI reject").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                            .frame(width: 72, alignment: .leading)
                        Text(note).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                            .fixedSize(horizontal: false, vertical: true)
                            .textSelection(.enabled)
                    }
                    .help("Why AI Cull rejected this photo. Changing the flag clears it.")
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel("AI Cull rejected it: \(note)")
                }
                if let s = model.cullSuggestions[p.id] {
                    Divider().padding(.vertical, DS.Space.xs)
                    HStack {
                        Label("AI Cull", systemImage: "sparkles").font(DS.Font.label)
                        Spacer()
                        Text("\(Safe.clampedInt((s.score * 100)))").font(DS.Font.value)
                    }
                    ForEach(s.reasons, id: \.self) { r in
                        Text("· \(r)").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                    }
                }
                Divider().padding(.vertical, DS.Space.xs)
                IPTCFieldsView(model: model)
            }
        }
    }
}

struct KeywordsPanel: View {
    @Bindable var model: AppModel
    @State private var entry = ""

    var body: some View {
        let keywords = model.selectionKeywords
        let count = model.selectedIDs.count
        VStack(alignment: .leading, spacing: DS.Space.s) {
            TextField(count > 1 ? "Add to \(count) photos (comma-separated)" : "Add keywords (comma-separated)", text: $entry)
                .textFieldStyle(.roundedBorder)
                .onSubmit {
                    model.addKeywords(entry)
                    entry = ""
                }
                .disabled(count == 0)
                .accessibilityLabel("Add keywords")
            if keywords.isEmpty {
                PanelNote(text: count == 0 ? "Select photos to add keywords." : "No keywords yet. They're saved with the photo and written into exports.")
            } else {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 72), spacing: DS.Space.xs)], alignment: .leading, spacing: DS.Space.xs) {
                    ForEach(keywords, id: \.self) { k in
                        Chip(title: "\(k)  ✕", selected: true) { model.removeKeyword(k) }
                            .help("Remove “\(k)”")
                            .accessibilityLabel("Keyword \(k)")
                            .accessibilityHint("Removes it")
                    }
                }
            }
            let library = KeywordLibrary.shared.suggestions(excluding: keywords)
            if count > 0, !library.isEmpty {
                Text("Keyword library (click to add)").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 72), spacing: DS.Space.xs)], alignment: .leading, spacing: DS.Space.xs) {
                    ForEach(library, id: \.self) { k in Chip(title: k, selected: false) { model.addKeywords(k) } }
                }
            }
            let reasons = model.focusedID.flatMap { model.cullSuggestions[$0]?.reasons } ?? []
            if !reasons.isEmpty {
                Text("From AI Cull (click to add)").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 72), spacing: DS.Space.xs)], alignment: .leading, spacing: DS.Space.xs) {
                    ForEach(reasons, id: \.self) { r in Chip(title: r, selected: false) { model.addKeywords(r) } }
                }
            }
        }
    }
}

/// [exp6] Export tool tab: the saved recipes and the ⌘E sheet.
struct ExportPanel: View {
    @Bindable var model: AppModel

    var body: some View {
        let recipes = ExportRecipeStore(defaults: model.settings.userDefaults).load()
        VStack(alignment: .leading, spacing: DS.Space.s) {
            ForEach(recipes) { r in
                Label {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(r.name).font(DS.Font.label)
                        Text(r.summary).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                    }
                } icon: {
                    Image(systemName: r.enabled ? "checkmark.circle.fill" : "circle")
                        .foregroundStyle(r.enabled ? DS.accent : DS.textSecondary)
                }
            }
            Button(model.selectedIDs.count > 1 ? "Export \(model.selectedIDs.count) Photos…" : "Export…") {
                model.beginExport()
            }
            .controlSize(.small)
            .disabled(!CommandCatalog.isEnabled(.exportSelected, model: model))
            .help("Export the selected photos (⌘E)")
        }
    }
}

struct AmazonPhotosPanel: View {
    @Bindable var settings: AppSettings

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            PanelNote(text: "Darkslide mirrors your originals into a folder that the official Amazon Photos app backs up. No passwords, no private APIs.")
            InfoRow(label: "Folder", value: settings.amazonPhotosFolder?.path ?? "Off")
            HStack {
                Button("Choose…") {
                    if let url = PlatformPanels.chooseItem(files: false, directories: true, createDirectories: true,
                                                           prompt: "Use Folder",
                                                           message: "Choose the folder the Amazon Photos app backs up") {
                        settings.amazonPhotosFolder = url
                    }
                }
                .help("Pick the folder watched by the Amazon Photos app")
                if settings.amazonPhotosFolder != nil {
                    Button("Turn Off") { settings.amazonPhotosFolder = nil }
                        .help("Stop mirroring originals to Amazon Photos")
                }
            }
            .controlSize(.small)
        }
    }
}
