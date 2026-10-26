import LuxtableCore
import SwiftUI

/// People masks in the Layers panel: a person picker ("All People", "Person 1…n") and
/// the Person, Face, Skin, Eyes, Teeth, Hair and Clothes targets. Logic lives in
/// `PeopleMenu` and `LayerController.addPeopleMask`.
struct PeopleMaskMenu: View {
    @Bindable var model: AppModel
    let controller: LayerController
    @State private var person: Int?
    @State private var count = 0
    @State private var busy: AITarget?
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            Picker("People", selection: $person) {
                ForEach(PeopleMenu.personChoices(count: count), id: \.label) { c in
                    Text(c.label).tag(c.person)
                }
            }
            .controlSize(.small)
            .accessibilityLabel("Which person to mask")
            LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: DS.Space.xs), count: 4), spacing: DS.Space.xs) {
                ForEach(PeopleTargets.parts, id: \.self) { t in
                    Button {
                        Task { await run(t) }
                    } label: {
                        HStack(spacing: 3) {
                            if busy == t { ProgressView().controlSize(.mini) } else { Image(systemName: PeopleMenu.symbol(t)) }
                            Text(t.rawValue.capitalized).font(DS.Font.caption)
                        }
                        .frame(maxWidth: .infinity)
                    }
                    .controlSize(.small)
                    .disabled(busy != nil || model.focusedID == nil)
                    .help(PeopleMenu.help(t, person: person))
                    .accessibilityLabel("AI \(PeopleTargets.label(t, person: person)) mask")
                }
            }
            if let d = PeopleMenu.downloadStatus {
                HStack(alignment: .bottom, spacing: DS.Space.s) {
                    ProgressView(value: d.fraction) { Text(d.text).font(DS.Font.caption) }
                        .controlSize(.small)
                    Button("Cancel") { PeopleMenu.cancelDownload() }
                        .controlSize(.small)
                        .accessibilityLabel("Cancel the face model download")
                }
            }
            if let error {
                Label(error, systemImage: "exclamationmark.triangle").font(DS.Font.caption).foregroundStyle(DS.reject)
            }
        }
        .task(id: model.focusedID) {
            person = nil
            count = await controller.peopleCount()
        }
    }

    private func run(_ t: AITarget) async {
        busy = t
        error = nil
        defer { busy = nil }
        do {
            try await controller.addPeopleMask(t, person: person)
        } catch is CancellationError {
        } catch {
            self.error = error.localizedDescription
        }
    }
}
