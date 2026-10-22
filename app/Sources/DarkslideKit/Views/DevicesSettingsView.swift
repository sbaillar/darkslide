import SwiftUI

/// Settings ▸ Devices: allow iPad connections, pair by QR code or 6-digit code,
/// and the paired devices with write access and Revoke.
struct DevicesSettingsView: View {
    @Bindable var model: RemoteLinkModel

    var body: some View {
        Form {
            Section {
                Toggle("Allow iPad connections", isOn: Binding(get: { model.enabled }, set: { model.setEnabled($0) }))
                Text("While Darkslide is open, a paired iPad on this network or your tailnet can browse the open session, download previews and originals, and, with write access, send photos and edits back. It stays on, whenever Darkslide is open, until you turn it off.")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                if let port = model.port, let fp = model.fingerprint {
                    LabeledContent("This Mac") {
                        Text("port \(String(port)) · key \(fp)").font(.system(.caption, design: .monospaced))
                    }
                }
                if let error = model.error {
                    Text(error).font(DS.Font.label).foregroundStyle(DS.reject)
                }
            }
            Section("Pair a device") {
                if let p = model.pairing {
                    HStack(alignment: .top, spacing: DS.Space.m) {
                        if let img = model.qrImage {
                            Image(decorative: img, scale: 1).interpolation(.none).resizable()
                                .frame(width: 160, height: 160)
                                .accessibilityLabel("Pairing QR code")
                        }
                        VStack(alignment: .leading, spacing: DS.Space.s) {
                            Text("Scan this with Darkslide on the iPad, or type the code:")
                            Text(p.code).font(.system(size: 28, weight: .semibold, design: .monospaced))
                                .accessibilityLabel("Pairing code \(p.code.map(String.init).joined(separator: " "))")
                            Text("The code works once, for five minutes.")
                                .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                            Button("Cancel") { model.cancelPairing() }
                        }
                    }
                } else {
                    Button("Pair a Device…") { model.beginPairing() }
                        .disabled(!model.running)
                        .help(model.running ? "Show a QR code and a code to type" : "Turn on “Allow iPad connections” first")
                }
            }
            Section("Paired devices") {
                if model.devices.isEmpty {
                    Text("None yet").foregroundStyle(DS.textSecondary)
                }
                ForEach(model.devices) { d in
                    HStack {
                        VStack(alignment: .leading) {
                            Text(d.name)
                            Text(d.shortFingerprint).font(.system(.caption, design: .monospaced))
                                .foregroundStyle(DS.textSecondary)
                        }
                        Spacer()
                        Toggle("Can send photos and edits", isOn: Binding(get: { d.write }, set: { model.setWrite(d, $0) }))
                            .toggleStyle(.switch).controlSize(.small)
                        Button("Revoke", role: .destructive) { model.revoke(d) }
                            .accessibilityLabel("Revoke \(d.name)")
                    }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear { model.reloadDevices() }
    }
}

/// Toolbar: shown while a device is connected; animates during a transfer.
struct RemoteBadge: View {
    let remote: RemoteLinkModel

    var body: some View {
        if let i = remote.indicator {
            Image(systemName: i.symbol)
                .symbolEffect(.pulse, isActive: remote.status.transferring)
                .foregroundStyle(DS.accent)
                .help(i.label)
                .accessibilityLabel(i.label)
        }
    }
}
