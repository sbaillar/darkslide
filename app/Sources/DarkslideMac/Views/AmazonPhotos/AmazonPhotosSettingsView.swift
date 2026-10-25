import AppKit
import DarkslideCore
import DarkslideUI
import SwiftUI

/// Settings › Amazon Photos (shown while the integration is on in Settings › Plugins):
/// the folder the Amazon Photos app backs up, and whether rejects go there.
struct AmazonPhotosSettingsView: View {
    @State var model: ArchiveSettingsModel

    var body: some View {
        @Bindable var settings = model.settings
        Form {
            Section("Amazon Photos") {
                LabeledContent("Backed-up folder", value: settings.amazonPhotosFolder?.path ?? "Off")
                HStack {
                    Button("Choose…") {
                        if let url = pickFolder("Choose the folder the Amazon Photos app backs up") {
                            settings.chooseAmazonFolder(url)
                            Task { await model.refresh() }
                        }
                    }
                    if settings.amazonPhotosFolder != nil {
                        Button("Turn Off") { settings.chooseAmazonFolder(nil) }
                    }
                }
                Text(model.amazonText).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Text(ArchiveSettingsModel.amazonVideosNote).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Toggle("Send rejected photos to Amazon Photos", isOn: $settings.amazonRejected) // N2
                    .help("Off: photos you rejected aren't handed to Amazon Photos")
                if model.amazonAppInstalled == false {
                    Link("Get the Amazon Photos app", destination: ArchiveSettingsModel.amazonInstallURL)
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 520)
        .padding(DS.Space.m)
        .task { await model.refresh() }
    }

    private func pickFolder(_ message: String) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.message = message
        return panel.runModal() == .OK ? panel.url : nil
    }
}
