import DarkslideFFI
import SwiftUI

/// ⌘F Find: one field over the window ("let's find pics of alex in iceland"),
/// how it was understood, then two groups: sessions and folders (each with
/// Open) and photos across every session (opening one opens its session and
/// shows it). Every result says why it matched. Esc closes.
struct FindPanel: View {
    let model: AppModel
    @Bindable var find: FindModel
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            field
            if !find.summary.isEmpty {
                ChipRow(chips: find.summary)
            }
            ForEach(find.notes, id: \.self) { note in
                Label(note, systemImage: "info.circle").font(DS.Font.label).foregroundStyle(DS.textSecondary)
            }
            if let r = find.results {
                Divider()
                results(r)
            }
        }
        .padding(DS.Space.m)
        .frame(width: 760)
        .frame(maxHeight: 640, alignment: .top)
        .background { if model.snapshotMode { RoundedRectangle(cornerRadius: DS.Radius.l * 1.5).fill(DS.panelBackground) } }
        .glassEffect(.regular, in: .rect(cornerRadius: DS.Radius.l * 1.5))
        .shadow(color: .black.opacity(0.3), radius: 24, y: 8)
        .onAppear { focused = true }
    }

    private var field: some View {
        HStack(spacing: DS.Space.s) {
            Image(systemName: "magnifyingglass").foregroundStyle(DS.accent)
            TextField("Find — “pics of alex in iceland last summer”", text: $find.query)
                .textFieldStyle(.plain)
                .font(.system(size: 15))
                .focused($focused)
                .onSubmit { find.submit() }
                .onChange(of: find.query) { _, q in
                    if q.trimmingCharacters(in: .whitespaces).isEmpty { find.clear() } else { find.queryChanged() }
                }
                .onKeyPress(.escape) { find.dismiss(); return .handled }
                .accessibilityLabel("Find photos and sessions")
                .accessibilityHint("People, places, dates, ratings and descriptions, across every session.")
            switch find.phase {
            case .searching: ProgressView().controlSize(.small)
            case .failed(let m): Image(systemName: "exclamationmark.triangle").foregroundStyle(DS.warningText).help(m)
            case .idle, .results: EmptyView()
            }
            Button {
                find.dismiss()
            } label: {
                Image(systemName: "xmark")
            }
            .buttonStyle(.borderless)
            .help("Close (Esc)")
            .accessibilityLabel("Close Find")
        }
    }

    @ViewBuilder private func results(_ r: FindResults) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: DS.Space.m) {
                if !r.sessions.isEmpty {
                    Text("Sessions & Folders").font(.headline)
                    ForEach(r.sessions, id: \.root) { s in
                        HStack {
                            Image(systemName: "folder").foregroundStyle(DS.textSecondary)
                            VStack(alignment: .leading) {
                                Text(s.name).fontWeight(.semibold)
                                Text(FindModel.reason(s)).font(DS.Font.label).foregroundStyle(DS.textSecondary)
                            }
                            Spacer()
                            if !s.available { Text("offline").font(DS.Font.label).foregroundStyle(DS.warningText) }
                            Button("Open") { Task { await find.open(s) } }
                                .disabled(!s.available)
                                .accessibilityLabel("Open session \(s.name)")
                        }
                    }
                }
                HStack {
                    Text("Photos").font(.headline)
                    Text(r.total == 1 ? "1 match" : "\(r.total.formatted()) matches").foregroundStyle(DS.textSecondary)
                    if r.total > UInt64(r.photos.count) {
                        Text("(best \(r.photos.count) shown)").foregroundStyle(DS.textSecondary)
                    }
                }
                if r.photos.isEmpty {
                    Text("Nothing matches.").foregroundStyle(DS.textSecondary)
                }
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 132), spacing: DS.Space.s)], spacing: DS.Space.s) {
                    ForEach(r.photos, id: \.path) { p in
                        FindPhotoCell(model: model, photo: p, chips: FindModel.chips(p, phrase: r.query.date?.phrase))
                            .onTapGesture { Task { await find.open(p) } }
                    }
                }
            }
        }
    }
}

/// A result photo: its preview, session, and why it matched.
private struct FindPhotoCell: View {
    let model: AppModel
    let photo: FindPhotoRecord
    let chips: [FindModel.Chip]
    @State private var image: CGImage?

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Group {
                if let image {
                    Image(decorative: image, scale: 1).resizable().scaledToFill()
                } else {
                    Color.primary.opacity(0.08)
                }
            }
            .frame(width: 132, height: 92)
            .clipShape(RoundedRectangle(cornerRadius: DS.Radius.s))
            Text(photo.sessionName).font(DS.Font.caption).foregroundStyle(DS.textSecondary).lineLimit(1)
            ForEach(chips.prefix(3), id: \.self) { c in
                ChipView(chip: c).lineLimit(1)
            }
        }
        .frame(width: 132, alignment: .leading)
        .contentShape(.rect)
        .help(URL(fileURLWithPath: photo.path).lastPathComponent)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(URL(fileURLWithPath: photo.path).lastPathComponent) in \(photo.sessionName)")
        .accessibilityAddTraits(.isButton)
        .task(id: photo.path) {
            image = try? await model.previews?.preview(for: URL(fileURLWithPath: photo.path), maxPixelSize: 264)
        }
    }
}

struct ChipRow: View {
    let chips: [FindModel.Chip]

    var body: some View {
        HStack(spacing: DS.Space.xs) {
            ForEach(chips, id: \.self) { ChipView(chip: $0) }
            Spacer(minLength: 0)
        }
    }
}

struct ChipView: View {
    let chip: FindModel.Chip

    var symbol: String {
        switch chip.kind {
        case .person: "person.fill"
        case .suggested: "person.fill.questionmark"
        case .place: "mappin.and.ellipse"
        case .looksLike: "eye"
        case .date: "calendar"
        case .rating: "star"
        case .flag: "flag"
        case .visual: "sparkle.magnifyingglass"
        case .text: "text.magnifyingglass"
        }
    }

    var body: some View {
        Label(chip.text, systemImage: symbol)
            .font(DS.Font.caption)
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(Capsule().fill(Color.primary.opacity(0.08)))
            .accessibilityLabel(chip.text)
    }
}
