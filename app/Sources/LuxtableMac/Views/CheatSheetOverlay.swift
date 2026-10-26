import LuxtableCore
import LuxtableUI
import SwiftUI

/// `?` overlay listing every shortcut. Click anywhere or press Esc/? to close.
struct CheatSheetOverlay: View {
    /// The user's current single-key shortcuts (the sheet shows them, not the defaults).
    var bindings: KeyBindings = .defaults
    let dismiss: () -> Void

    var body: some View {
        ZStack {
            Color.black.opacity(0.35).ignoresSafeArea().onTapGesture(perform: dismiss)
            VStack(alignment: .leading, spacing: DS.Space.m) {
                HStack {
                    Image(systemName: "keyboard").font(.title2).foregroundStyle(DS.accent)
                    Text("Keyboard Shortcuts").font(DS.Font.title)
                    Spacer()
                    Button(action: dismiss) { Image(systemName: "xmark.circle.fill").font(.title2) }
                        .buttonStyle(.plain).foregroundStyle(DS.textSecondary)
                        .help("Close (Esc)")
                        .accessibilityLabel("Close keyboard shortcuts")
                }
                Grid(alignment: .topLeading, horizontalSpacing: DS.Space.xl, verticalSpacing: DS.Space.m) {
                    GridRow {
                        section(0)
                        section(1)
                    }
                    GridRow {
                        section(2)
                        section(3)
                    }
                    GridRow {
                        section(4)
                        section(5)
                    }
                }
                Text("Culling keys work in the viewer and the grid. Text fields keep their own keys.")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
            .padding(DS.Space.l)
            .frame(width: 620)
            .glassEffect(.regular, in: .rect(cornerRadius: DS.Radius.l * 1.5))
            .shadow(color: .black.opacity(0.3), radius: 30, y: 10)
        }
    }

    private func section(_ i: Int) -> some View {
        let s = KeyboardRouter.cheatSheet(bindings: bindings)[i]
        return VStack(alignment: .leading, spacing: DS.Space.s) {
            Text(s.section.uppercased()).font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
            ForEach(Array(s.rows.enumerated()), id: \.offset) { _, row in
                HStack(spacing: DS.Space.m) {
                    Text(row.keys)
                        .font(.system(size: 12, weight: .medium, design: .rounded))
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(Color.primary.opacity(0.08), in: .rect(cornerRadius: DS.Radius.s))
                        .frame(width: 110, alignment: .leading)
                    Text(row.action).font(.system(size: 12))
                }
            }
        }
    }
}
