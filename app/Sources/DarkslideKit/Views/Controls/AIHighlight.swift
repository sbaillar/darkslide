import SwiftUI

/// A tool row an AI edit changed: a yellow outline over a faint yellow tint, a sparkles
/// badge, "AI changed it to …" on hover, Undo AI Change in its context menu, and "changed
/// by AI" after its VoiceOver value. Drawn outside the row's frame (in the panel's
/// padding), so highlighting never moves the layout. Contrast: `AIHighlightStyle` (tested).
struct AIHighlightRow: ViewModifier {
    let note: String?
    /// Draw the badge at the top-trailing corner (sliders draw it next to their label).
    var badge = true
    /// The control's VoiceOver value, to append "changed by AI" to (nil: leave it).
    var value: String?
    /// Put the row back at its value before AI (nil: not known).
    var undo: (() -> Void)?

    func body(content: Content) -> some View {
        if let note {
            let shape = RoundedRectangle(cornerRadius: DS.Radius.s)
            let marked = content
                .background {
                    shape.fill(DS.aiChange.opacity(AIHighlightStyle.tintOpacity))
                        .padding(.horizontal, -DS.Space.s)
                        .padding(.vertical, -DS.Space.xxs)
                }
                .overlay {
                    shape.strokeBorder(DS.aiChange, lineWidth: AIHighlightStyle.borderWidth)
                        .padding(.horizontal, -DS.Space.s)
                        .padding(.vertical, -DS.Space.xxs)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
                .overlay(alignment: .topTrailing) {
                    if badge { AIBadge().help(note) }
                }
                .help(note)
                .modifier(AIUndoMenu(undo: undo))
            if let value { marked.accessibilityValue(AIHighlightStyle.accessibilityValue(value)) } else { marked }
        } else {
            content
        }
    }
}

/// A changed row's context menu: Undo AI Change (none when its earlier value isn't known).
private struct AIUndoMenu: ViewModifier {
    let undo: (() -> Void)?

    func body(content: Content) -> some View {
        if let undo {
            content.contextMenu { Button(AIChangeText.undoOneTitle, action: undo) }
        } else {
            content
        }
    }
}

/// The small sparkles badge of an AI-changed row, panel header or tool tab.
struct AIBadge: View {
    var size: CGFloat = 9

    var body: some View {
        Image(systemName: AIHighlightStyle.badgeSymbol)
            .font(.system(size: size, weight: .semibold))
            .foregroundStyle(DS.aiChange)
            .accessibilityHidden(true)
    }
}

extension View {
    /// Highlight this row when the focused photo's AI edit changed `field`.
    func aiHighlight(_ model: AppModel, _ field: AIField, value: String? = nil) -> some View {
        modifier(AIHighlightRow(note: model.aiNote(field), value: value, undo: model.aiUndo(field)))
    }
}
