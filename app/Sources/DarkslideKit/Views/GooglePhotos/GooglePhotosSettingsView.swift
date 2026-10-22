import SwiftUI

/// Settings ▸ Google Photos: the user's own OAuth client, then sign in or out.
struct GooglePhotosSettingsView: View {
    @State private var model: GooglePhotosSettingsModel
    @State private var presenter: WebAuthPresenter

    init(settings: GooglePhotosSettings, service: any GooglePhotosService) {
        let presenter = WebAuthPresenter()
        _presenter = State(initialValue: presenter)
        _model = State(initialValue: GooglePhotosSettingsModel(settings: settings, service: service) {
            try await GooglePhotosSignIn(settings: settings, service: service, presenter: presenter).run()
        })
    }

    var body: some View {
        Form {
            Section("Account") {
                LabeledContent("Status", value: model.status)
                HStack {
                    if model.isSignedIn {
                        Button("Sign Out", role: .destructive) { Task { await model.signOut() } }
                    } else {
                        Button("Sign In with Google…") { Task { await model.signInNow() } }
                            .disabled(!model.canSignIn)
                    }
                    if model.busy { ProgressView().controlSize(.small) }
                }
                Text("Exports go to Google Photos only from recipes you turn it on for. Darkslide can add to albums it "
                    + "created and can't see the rest of your library.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                if let m = model.message { Text(m).font(DS.Font.caption).foregroundStyle(DS.textSecondary) }
            }
            if model.hasBuiltInClient {
                Section {
                    DisclosureGroup("Use your own Google OAuth client (advanced)") { ownClient }
                }
            } else {
                Section("Your OAuth client") { ownClient }
                Section("How to set up") { setupSteps }
            }
        }
        .formStyle(.grouped)
    }

    @ViewBuilder private var ownClient: some View {
        TextField("Client ID", text: $model.clientIDEntry)
            .disabled(model.isSignedIn)
            .accessibilityLabel("Google OAuth client ID")
        if model.hasSecret {
            Label("Client secret saved in your Keychain", systemImage: "key.fill")
        } else {
            SecureField("Client secret", text: $model.secretEntry)
                .disabled(model.isSignedIn)
                .accessibilityLabel("Google OAuth client secret")
        }
        HStack {
            Button("Save") { model.saveClient() }.disabled(!model.canSaveClient)
            if !model.ownClientID.isEmpty {
                Button("Remove Client") { model.removeClient() }.disabled(model.isSignedIn)
            }
        }
        if model.hasBuiltInClient {
            Text("Sign out first. Your client replaces Darkslide's own until you remove it (docs/GOOGLE_PHOTOS.md).")
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
        }
    }

    @ViewBuilder private var setupSteps: some View {
        ForEach(Array(GooglePhotosSettingsModel.setupSteps.enumerated()), id: \.offset) { i, step in
            Text("\(i + 1). \(step)").font(DS.Font.caption)
        }
        Text("Every step is in docs/GOOGLE_PHOTOS.md. This build has no Google client built in.")
            .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
    }
}
