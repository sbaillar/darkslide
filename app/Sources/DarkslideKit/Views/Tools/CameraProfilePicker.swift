import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// The edit's base look: Darkslide Standard / Neutral / Vivid and imported Adobe
/// camera profiles, plus "Import Camera Profile…" (`CameraProfile.swift`).
struct CameraProfilePicker: View {
    @Bindable var model: AppModel
    @State private var options: [CameraProfileOption] = []
    @State private var importError: String?

    var body: some View {
        let current = model.focusedEdit.profile
        let camera = model.focusedPhoto?.meta.camera ?? ""
        HStack(spacing: DS.Space.s) {
            Text("Profile").font(DS.Font.label).foregroundStyle(DS.textSecondary)
            Menu {
                ForEach(options.filter { !$0.isImported }) { o in choice(o, current) }
                // M2: profiles made for another camera are listed apart and can't be
                // picked (they would render as Standard).
                let imported = options.filter(\.isImported)
                let fitting = imported.filter { CameraProfileLibrary.fits($0.profile, camera: camera) }
                if !fitting.isEmpty {
                    Divider()
                    ForEach(fitting) { o in choice(o, current) }
                }
                let others = imported.filter { !fitting.contains($0) }
                if !others.isEmpty {
                    Divider()
                    Section("Other cameras") {
                        ForEach(others) { o in
                            choice(o, current, label: o.camera.map { "\(o.name) (\($0))" } ?? o.name)
                                .disabled(o.profile != current)
                        }
                    }
                }
                Divider()
                Button("Import Camera Profile…") { importProfile() }
            } label: {
                Text(options.first { $0.profile == current }?.name ?? current.displayName)
                    .lineLimit(1)
            }
            .accessibilityLabel("Camera profile")
        }
        .onAppear { options = CameraProfileLibrary.options(current: current) }
        .onChange(of: current) { _, c in options = CameraProfileLibrary.options(current: c) }
        .alert("Couldn't import the profile", isPresented: Binding(
            get: { importError != nil }, set: { if !$0 { importError = nil } }
        )) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(importError ?? "")
        }
    }

    private func choice(_ o: CameraProfileOption, _ current: CameraProfileRef, label: String? = nil) -> some View {
        Button {
            model.setProfile(o.profile)
        } label: {
            let name = label ?? o.name
            if o.profile == current { Label(name, systemImage: "checkmark") } else { Text(name) }
        }
    }

    private func importProfile() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [UTType(filenameExtension: "dcp") ?? .data]
        panel.allowsMultipleSelection = false
        panel.message = "Choose an Adobe camera profile (.dcp), e.g. from Adobe DNG Converter."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        Task {
            do {
                let o = try await CameraProfileLibrary.importProfile(at: url)
                model.setProfile(o.profile)
                options = CameraProfileLibrary.options(current: o.profile)
            } catch {
                importError = error.localizedDescription
            }
        }
    }
}
