import SwiftUI

/// "New Smart Folder…" / "Edit Smart Folder…": any combination of rules.
struct SmartFolderEditor: View {
    let target: SmartFolderEditorTarget
    let onSave: (SmartFolderDraft) -> Void
    @State private var draft: SmartFolderDraft
    @Environment(\.dismiss) private var dismiss

    init(target: SmartFolderEditorTarget, onSave: @escaping (SmartFolderDraft) -> Void) {
        self.target = target
        self.onSave = onSave
        _draft = State(initialValue: target.draft)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text(target == .new ? "New Smart Folder" : "Edit Smart Folder").font(.headline)
            Form {
                TextField("Name", text: $draft.name)
                HStack {
                    Picker("Rating", selection: $draft.ratingMode) {
                        ForEach(SmartFolderDraft.RatingMode.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                    }
                    if ![.any, .unrated].contains(draft.ratingMode) {
                        Stepper("\(draft.rating)★", value: $draft.rating, in: 1...5).fixedSize()
                    }
                    if draft.ratingMode == .between {
                        Text("and")
                        Stepper("\(draft.ratingHigh)★", value: $draft.ratingHigh, in: 1...5).fixedSize()
                    }
                }
                LabeledContent("Color tags") {
                    HStack(spacing: DS.Space.xs) {
                        ForEach(ColorTag.allCases.filter { $0 != .none }, id: \.self) { t in
                            let on = draft.tags.contains(t)
                            Button {
                                if on { draft.tags.remove(t) } else { draft.tags.insert(t) }
                            } label: {
                                Circle().fill(DS.tagColor(t)).frame(width: 14, height: 14)
                                    .overlay(Circle().strokeBorder(.white, lineWidth: on ? 2 : 0))
                            }
                            .buttonStyle(.plain)
                            .accessibilityLabel("\(t.name) tag")
                            .accessibilityAddTraits(on ? .isSelected : [])
                        }
                    }
                }
                Picker("Flag", selection: $draft.pick) {
                    Text("Any").tag(PickState?.none)
                    Text("Picked").tag(PickState?.some(.picked))
                    Text("Rejected").tag(PickState?.some(.rejected))
                    Text("Unflagged").tag(PickState?.some(PickState.none))
                }
                TextField("Camera contains", text: $draft.camera)
                TextField("Lens contains", text: $draft.lens)
                HStack {
                    Toggle("From", isOn: $draft.useDateFrom)
                    DatePicker("", selection: $draft.dateFrom, displayedComponents: .date).labelsHidden().disabled(!draft.useDateFrom)
                }
                HStack {
                    Toggle("To", isOn: $draft.useDateTo)
                    DatePicker("", selection: $draft.dateTo, displayedComponents: .date).labelsHidden().disabled(!draft.useDateTo)
                }
                HStack {
                    TextField("ISO from", text: $draft.isoMin)
                    TextField("to", text: $draft.isoMax)
                }
                TextField("Keywords (all of, comma-separated)", text: $draft.keywords)
                TextField("Text (name, keywords, camera, lens)", text: $draft.text)
                Picker("Edited", selection: $draft.edited) {
                    ForEach(SmartFolderDraft.EditedMode.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                }
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Save") {
                    onSave(draft)
                    dismiss()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(!draft.canSave)
            }
        }
        .padding(DS.Space.l)
        .frame(width: 440)
    }
}
