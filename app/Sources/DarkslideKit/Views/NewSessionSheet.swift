import AppKit
import SwiftUI

/// File ▸ New Session…: a name and where the session goes (the sessions folder, or one
/// chosen). ⏎ creates, Esc cancels.
struct NewSessionSheet: View {
    @Bindable var model: AppModel
    @State private var form: NewSessionModel
    @FocusState private var nameFocused: Bool

    init(model: AppModel) {
        self.model = model
        _form = State(initialValue: NewSessionModel(app: model))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text("New Session").font(.headline).foregroundStyle(DS.text)
            VStack(alignment: .leading, spacing: DS.Space.xs) {
                Text("Name").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                TextField("Session name", text: $form.name)
                    .textFieldStyle(.roundedBorder)
                    .focused($nameFocused)
                    .onSubmit { create() }
                    .accessibilityLabel("Session name")
                    .accessibilityHint(form.problem ?? "Return creates the session")
                if let problem = form.problem {
                    Label {
                        Text(problem).foregroundStyle(DS.text)
                    } icon: {
                        Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(DS.warningText)
                    }
                    .font(DS.Font.label)
                    .accessibilityElement(children: .combine)
                }
            }
            VStack(alignment: .leading, spacing: DS.Space.xs) {
                Text("Location").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                HStack {
                    Label(form.location.map { AppModel.displayPath($0) } ?? "Finding the sessions folder…",
                          systemImage: "folder")
                        .font(DS.Font.label)
                        .foregroundStyle(DS.text)
                        .lineLimit(1).truncationMode(.middle)
                        .help(form.location?.path ?? "")
                        .accessibilityLabel("Location: \(form.location?.path ?? "unknown")")
                    Spacer()
                    Button("Choose…", action: chooseLocation)
                        .disabled(form.isCreating)
                        .help("Create the session in another folder")
                }
                if let warning = form.locationWarning {
                    Label {
                        Text(warning).foregroundStyle(DS.text).fixedSize(horizontal: false, vertical: true)
                    } icon: {
                        Image(systemName: "icloud").foregroundStyle(DS.warningText)
                    }
                    .font(DS.Font.caption)
                }
                Text("An empty session with Capture, Selects, Output and Trash folders. Import from a card or drag photos in next.")
                    .font(DS.Font.caption)
                    .foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                if form.isCreating { ProgressView().controlSize(.small) }
                Spacer()
                Button("Cancel", role: .cancel) { model.sheet = nil }
                    .keyboardShortcut(.cancelAction)
                    .disabled(form.isCreating) // the session is being made and opened
                Button("Create") { create() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!form.canCreate)
                    .help(form.problem ?? "Create the session and open it")
            }
        }
        .padding(DS.Space.l)
        .frame(width: 440)
        .background(DS.panelBackground)
        .task {
            nameFocused = true
            await form.load()
        }
    }

    private func chooseLocation() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.directoryURL = form.location
        panel.prompt = "Choose"
        panel.message = "Choose where to create the session"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        Task { await form.choose(url) }
    }

    private func create() {
        guard form.canCreate else { return }
        Task {
            if await form.create(), model.sheet == .newSession { model.sheet = nil }
        }
    }
}
