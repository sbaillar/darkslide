import SwiftUI

/// Settings ▸ AI: the typed-edit backends (rules, Jev, Apple Intelligence).
struct AISettingsView: View {
    @Bindable var model: AISettingsModel

    var body: some View {
        Form {
            Section("Edit with words (⌘K)") {
                Text("Typed edits change only the controls you name. Phrases Darkslide's rules don't understand go to the next helper, and anything still unclear is reported, never guessed.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                LabeledContent("Order", value: model.order)
            }
            Section("TypeSafe Jev") {
                Toggle("Use Jev for phrases the rules don't understand", isOn: $model.jevEnabled)
                Text("Only the text you type is sent to TypeSafe; photos never leave this Mac.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                if model.hasKey {
                    HStack {
                        Label("API key saved in your Keychain", systemImage: "key.fill")
                        Spacer()
                        Button("Remove Key", role: .destructive) { model.removeKey() }
                    }
                } else {
                    HStack {
                        SecureField("API key", text: $model.keyEntry)
                            .onSubmit { model.saveKey() }
                            .accessibilityLabel("TypeSafe API key")
                        Button("Save") { model.saveKey() }.disabled(model.keyEntry.isEmpty)
                    }
                }
                if let m = model.message { Text(m).font(DS.Font.caption).foregroundStyle(DS.textSecondary) }
            }
        }
        .formStyle(.grouped)
    }
}
