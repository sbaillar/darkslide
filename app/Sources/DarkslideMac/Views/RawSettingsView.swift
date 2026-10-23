import DarkslideCore
import DarkslideUI
import SwiftUI

/// Settings ▸ RAW: the rawler helper (download, status, remove) and what it's for.
struct RawSettingsView: View {
    var helper: RawlerHelper

    var body: some View {
        Form {
            Section {
                Text("Darkslide develops DNG files (including iPhone ProRAW) with its own decoders. Other RAW formats, and the colour data of most cameras, come from the free RAW helper: a separate program that runs the open-source rawler decoder. It isn’t part of Darkslide because it’s under another licence (LGPL-2.1).")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
                Text("You need it for Canon CR3 and CR2, Fujifilm RAF, Olympus/OM ORF, Panasonic RW2, Sony ARW and Nikon NEF files, and for HDR and panorama merges. Without it, those photos show their embedded previews: you can still cull, rate and tag them.")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Section("RAW helper") {
                LabeledContent("Status") {
                    if case .downloading(let f) = helper.state {
                        ProgressView(value: f).frame(width: 160)
                    } else {
                        Text(RawlerHelperText.status(helper.state))
                    }
                }
                HStack {
                    switch helper.state {
                    case .downloading:
                        Button("Cancel") { helper.cancel() }
                    case .installed:
                        Button("Remove RAW Helper") { helper.remove() }
                    case .notInstalled, .failed:
                        Button("Download RAW Helper (\(RawlerHelperText.size))") {
                            Task { try? await helper.download() }
                        }
                    }
                }
                Text("Downloaded from Darkslide’s releases on GitHub and checked against the fingerprint built into this version. Source, licence and how to build it yourself: crates/ds-rawler in Darkslide’s repository.")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .formStyle(.grouped)
    }
}
