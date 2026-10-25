import DarkslideCore
import SwiftUI

/// Soft proof settings under the histogram while it's on: profile, intent, black point
/// compensation and the gamut warning (logic in `SoftProofModel`). The switch itself
/// sits with Clipping and Focus peaking.
struct SoftProofControls: View {
    let proof: SoftProofModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            if proof.enabled {
                Picker("Profile", selection: Binding(get: { proof.settings.profile }, set: { $0.map(proof.setProfile) })) {
                    if let p = proof.settings.profile, !proof.profiles.contains(p) { Text(p.name).tag(Optional(p)) }
                    ForEach(proof.profileGroups, id: \.kind) { group in
                        Section(group.kind.title) {
                            ForEach(group.profiles) { Text($0.name).tag(Optional($0)) }
                        }
                    }
                }
                if proof.intentsApply {
                    Picker("Intent", selection: Binding(get: { proof.settings.intent }, set: { proof.setIntent($0) })) {
                        ForEach(ProofIntent.allCases, id: \.self) { Text($0.title).tag($0) }
                    }
                }
                Toggle("Black point compensation", isOn: Binding(get: { proof.settings.blackPointCompensation },
                                                                 set: { proof.setBlackPointCompensation($0) }))
                    .toggleStyle(.checkbox)
                HStack {
                    Toggle("Gamut warning", isOn: Binding(get: { proof.settings.gamutWarning },
                                                          set: { proof.setGamutWarning($0) }))
                        .toggleStyle(.checkbox)
                        .help("Paint colours the profile can't reproduce (relative colorimetric, without black "
                            + "point compensation: with BPC on, shadows below the paper's black still print, compressed)")
                    Picker("Color", selection: Binding(get: { proof.settings.warningColor }, set: { proof.setWarningColor($0) })) {
                        ForEach(GamutWarningColor.allCases, id: \.self) { Text($0.title).tag($0) }
                    }
                    .labelsHidden()
                    .fixedSize()
                    .disabled(!proof.settings.gamutWarning)
                }
                if let e = proof.error {
                    Text(e).foregroundStyle(DS.warningText)
                }
            }
        }
        .font(DS.Font.caption)
        .foregroundStyle(DS.textSecondary)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
