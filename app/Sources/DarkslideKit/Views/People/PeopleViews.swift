import DarkslideFFI
import SwiftUI

/// Library ▸ People, Lightroom-style: named people (click to show only their
/// photos), and groups of faces nobody has named yet ("Who is this?", "Is this
/// Alex?" with Yes/No). Select groups to merge them; open a group or a person
/// to split faces off ("Not Alex").
struct PeoplePanel: View {
    let model: AppModel
    @Bindable var people: PeopleModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            status
            if let e = people.error {
                Label(e, systemImage: "exclamationmark.triangle").font(DS.Font.label).foregroundStyle(DS.warningText)
            }
            if let o = people.overview {
                if !o.people.isEmpty {
                    Text("People").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    ForEach(o.people, id: \.name) { p in
                        PersonRow(model: model, people: people, person: p)
                    }
                }
                if !o.unnamed.isEmpty {
                    HStack {
                        Text("Who is this?").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                        Spacer()
                        if people.selected.count >= 2 {
                            Button("Merge \(people.selected.count) Groups") { Task { await people.mergeSelected() } }
                                .help("They're the same person")
                        }
                    }
                    ForEach(Array(o.unnamed.enumerated()), id: \.element.faces.first?.id) { i, c in
                        ClusterRow(model: model, people: people, cluster: c, index: i)
                    }
                }
                if o.people.isEmpty && o.unnamed.isEmpty && people.indexing == nil {
                    Text("No faces found in this session.").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                }
            } else {
                Text("Faces are found in the background, on this Mac.").font(DS.Font.label).foregroundStyle(DS.textSecondary)
            }
        }
    }

    @ViewBuilder private var status: some View {
        if let m = people.indexingMessage {
            HStack(spacing: DS.Space.xs) {
                ProgressView().controlSize(.mini)
                Text(m).font(DS.Font.label).foregroundStyle(DS.textSecondary)
            }
        } else if let s = people.overview?.status, s.faces > 0 {
            Text("\(s.faces.formatted()) faces in \(s.scanned.formatted()) photos")
                .font(DS.Font.label).foregroundStyle(DS.textSecondary)
        }
    }
}

/// A named person: their face, name and photo count; click to filter.
private struct PersonRow: View {
    let model: AppModel
    let people: PeopleModel
    let person: PersonGroup
    @State private var renaming = false
    @State private var newName = ""
    @State private var expanded = false
    @State private var picked: Set<FaceId> = []

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            HStack {
                if let f = person.faces.first {
                    FaceThumb(model: model, face: f, side: 32)
                } else {
                    Image(systemName: "person.crop.circle").font(.title2).foregroundStyle(DS.textSecondary)
                }
                Text(person.name).fontWeight(model.filter.person == person.name ? .bold : .regular)
                Text("\(person.photos)").foregroundStyle(DS.textSecondary)
                Spacer()
                if !person.faces.isEmpty {
                    Button { expanded.toggle() } label: { Image(systemName: expanded ? "chevron.up" : "chevron.down") }
                        .buttonStyle(.borderless)
                        .accessibilityLabel(expanded ? "Hide \(person.name)'s faces" : "Show \(person.name)'s faces")
                }
            }
            .contentShape(.rect)
            .onTapGesture { people.show(person) }
            .contextMenu {
                Button(model.filter.person == person.name ? "Show Everyone" : "Show Only \(person.name)") { people.show(person) }
                if person.personId != nil {
                    Button("Rename…") {
                        newName = person.name
                        renaming = true
                    }
                }
            }
            .help("Show only photos of \(person.name)")
            .accessibilityElement(children: .combine)
            .accessibilityAddTraits(.isButton)
            .popover(isPresented: $renaming) {
                HStack {
                    TextField("Name", text: $newName).frame(width: 160)
                        .onSubmit {
                            renaming = false
                            Task { await people.rename(person, to: newName) }
                        }
                    Button("Rename") {
                        renaming = false
                        Task { await people.rename(person, to: newName) }
                    }
                }
                .padding(DS.Space.s)
            }
            if expanded {
                FaceGrid(model: model, faces: person.faces, picked: $picked)
                if !picked.isEmpty {
                    Button("Not \(person.name) (\(picked.count))") {
                        let faces = person.faces.filter { picked.contains($0.id) }
                        picked = []
                        Task { await people.remove(faces, from: person) }
                    }
                }
            }
        }
    }
}

/// Faces nobody has named: "Is this …?", "Who is this?", select to merge,
/// open to split.
private struct ClusterRow: View {
    let model: AppModel
    @Bindable var people: PeopleModel
    let cluster: FaceCluster
    let index: Int
    @State private var name = ""
    @State private var expanded = false
    @State private var picked: Set<FaceId> = []

    private var isSelected: Binding<Bool> {
        Binding(get: { people.selected.contains(index) },
                set: { if $0 { people.selected.insert(index) } else { people.selected.remove(index) } })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            HStack(spacing: DS.Space.xs) {
                Toggle("", isOn: isSelected).toggleStyle(.checkbox).labelsHidden()
                    .accessibilityLabel("Select this group to merge")
                ForEach(cluster.faces.prefix(4), id: \.id) { f in FaceThumb(model: model, face: f, side: 32) }
                Text("\(cluster.faces.count)").foregroundStyle(DS.textSecondary)
                Spacer()
                Button { expanded.toggle() } label: { Image(systemName: expanded ? "chevron.up" : "chevron.down") }
                    .buttonStyle(.borderless)
                    .accessibilityLabel(expanded ? "Hide faces" : "Show faces")
            }
            if let s = cluster.suggestion {
                HStack {
                    Text("Is this \(s.name)?")
                    Button("Yes") { Task { await people.confirm(cluster) } }
                    Button("No") { Task { await people.reject(cluster) } }
                }
                .font(DS.Font.label)
            }
            HStack {
                TextField("Who is this?", text: $name)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit { submit() }
                    .accessibilityLabel("Name these faces")
                if let first = people.completions(for: name).first {
                    Button(first) {
                        name = first
                        submit()
                    }
                    .buttonStyle(.borderless)
                    .help("Name them \(first)")
                }
            }
            if expanded {
                FaceGrid(model: model, faces: cluster.faces, picked: $picked)
                if !picked.isEmpty, picked.count < cluster.faces.count {
                    Button("Split Off \(picked.count)") {
                        let faces = cluster.faces.filter { picked.contains($0.id) }
                        picked = []
                        Task { await people.split(faces, from: cluster) }
                    }
                    .help("These faces are someone else")
                }
            }
        }
        .padding(DS.Space.xs)
        .background(RoundedRectangle(cornerRadius: DS.Radius.s).fill(Color.primary.opacity(0.04)))
    }

    private func submit() {
        let n = name
        name = ""
        Task { await people.name(cluster, as: n) }
    }
}

/// Faces to pick (click toggles); double-click shows the photo.
private struct FaceGrid: View {
    let model: AppModel
    let faces: [FaceRecord]
    @Binding var picked: Set<FaceId>

    var body: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 44), spacing: 4)], spacing: 4) {
            ForEach(faces, id: \.id) { f in
                FaceThumb(model: model, face: f, side: 44)
                    .overlay(RoundedRectangle(cornerRadius: DS.Radius.s)
                        .stroke(picked.contains(f.id) ? DS.accent : .clear, lineWidth: 2))
                    .onTapGesture(count: 2) { model.focus(PhotoID(f.variantId), extend: false) }
                    .onTapGesture {
                        if picked.contains(f.id) { picked.remove(f.id) } else { picked.insert(f.id) }
                    }
                    .accessibilityAddTraits(picked.contains(f.id) ? [.isSelected, .isButton] : .isButton)
                    .accessibilityLabel("Face in \(URL(fileURLWithPath: f.path).lastPathComponent)")
            }
        }
    }
}

/// A face cropped from its photo's preview.
struct FaceThumb: View {
    let model: AppModel
    let face: FaceRecord
    let side: CGFloat
    @State private var image: CGImage?

    var body: some View {
        Group {
            if let image {
                Image(decorative: image, scale: 1).resizable().scaledToFill()
            } else {
                Color.primary.opacity(0.08)
            }
        }
        .frame(width: side, height: side)
        .clipShape(RoundedRectangle(cornerRadius: DS.Radius.s))
        .task(id: face.id) {
            guard let preview = try? await model.previews?.preview(for: URL(fileURLWithPath: face.path), maxPixelSize: 800)
            else { return }
            image = FaceThumb.crop(preview, face.rect)
        }
    }

    /// The face's square (1.3× its box), clamped to the image.
    static func crop(_ image: CGImage, _ r: FaceRect) -> CGImage? {
        let w = CGFloat(image.width), h = CGFloat(image.height)
        let side = max(CGFloat(r.w) * w, CGFloat(r.h) * h) * 1.3
        let cx = (CGFloat(r.x) + CGFloat(r.w) / 2) * w, cy = (CGFloat(r.y) + CGFloat(r.h) / 2) * h
        let rect = CGRect(x: cx - side / 2, y: cy - side / 2, width: side, height: side)
            .intersection(CGRect(x: 0, y: 0, width: w, height: h)).integral
        return rect.isEmpty ? nil : image.cropping(to: rect)
    }
}
