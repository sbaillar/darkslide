import DarkslideCore
import SwiftUI

/// The ⌘K "Edit with words" bar: a text field over the viewer with suggestions.
/// ⏎ applies (the typed text, or the highlighted suggestion), ↑/↓ move, Esc closes.
package struct CommandBarView: View {
    let model: AppModel
    @Bindable var bar: CommandBarModel
    @FocusState private var focused: Bool

    package init(model: AppModel, bar: CommandBarModel) {
        self.model = model
        self.bar = bar
    }

    package var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack(spacing: DS.Space.s) {
                Image(systemName: "text.bubble").foregroundStyle(DS.accent)
                TextField("Edit with words — “warmer and a bit brighter”", text: $bar.text)
                    .textFieldStyle(.plain)
                    .font(.system(size: 15))
                    .focused($focused)
                    .onSubmit { Task { await bar.submit(to: model) } }
                    .onKeyPress(.upArrow) { bar.moveHighlight(by: -1); return .handled }
                    .onKeyPress(.downArrow) { bar.moveHighlight(by: 1); return .handled }
                    .onKeyPress(.escape) { bar.dismiss(); return .handled }
                    .onChange(of: bar.text) { bar.highlighted = 0 }
                    .accessibilityLabel("Edit with words")
                    .accessibilityHint("Type an edit and press Return. Command-Z undoes it.")
                if bar.isRunning { ProgressView().controlSize(.small) }
                Text("⌘K").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
            }
            let items = bar.filtered
            if !items.isEmpty {
                Divider()
                ForEach(Array(items.enumerated()), id: \.offset) { i, s in
                    Button {
                        Task { await bar.submit(to: model, text: s) }
                    } label: {
                        HStack {
                            Image(systemName: "sparkles").font(.caption).foregroundStyle(DS.textSecondary)
                            Text(s)
                            Spacer()
                        }
                        .padding(.vertical, 3).padding(.horizontal, 6)
                        .background(i == bar.highlighted ? DS.accent.opacity(0.22) : .clear, in: .rect(cornerRadius: DS.Radius.s))
                        .contentShape(.rect)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Suggestion: \(s)")
                }
            }
            Text("Only what you name changes. Unclear words are reported, never guessed.")
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
        }
        .padding(DS.Space.m)
        .frame(width: 520)
        // `--snapshot`: window captures can't draw the glass, so a solid panel behind it.
        .background { if model.snapshotMode { RoundedRectangle(cornerRadius: DS.Radius.l * 1.5).fill(DS.panelBackground) } }
        .glassEffect(.regular, in: .rect(cornerRadius: DS.Radius.l * 1.5))
        .shadow(color: .black.opacity(0.3), radius: 24, y: 8)
        .onAppear { focused = true }
    }
}
