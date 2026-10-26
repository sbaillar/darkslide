import SwiftUI

/// Toolbar button look: icon (+ optional title) on a subtle capsule; `isOn`
/// fills it with the accent; disabled drops to a dashed outline at low opacity
/// so it can't be mistaken for an enabled item on dark chrome.
package struct ToolbarPillStyle: ButtonStyle {
    var showsTitle = false
    var isOn = false
    /// Not-yet-available features (live Import): outlined, muted text, still clickable to explain.
    var isSecondary = false
    @Environment(\.isEnabled) private var isEnabled

    package init(showsTitle: Bool = false, isOn: Bool = false, isSecondary: Bool = false) {
        self.showsTitle = showsTitle
        self.isOn = isOn
        self.isSecondary = isSecondary
    }

    package func makeBody(configuration: Configuration) -> some View {
        Group {
            if showsTitle {
                configuration.label.labelStyle(.titleAndIcon)
            } else {
                configuration.label.labelStyle(.iconOnly)
            }
        }
        .font(.system(size: 12, weight: .medium))
        .padding(.horizontal, showsTitle ? 10 : 8)
        .padding(.vertical, 4)
        .foregroundStyle(isOn ? Color.black : (isEnabled ? (isSecondary ? DS.textSecondary : Color.primary) : Color.primary.opacity(0.3)))
        .background {
            Capsule().fill(isOn ? DS.accent
                : Color.primary.opacity(!isEnabled || isSecondary ? (configuration.isPressed ? 0.1 : 0) : (configuration.isPressed ? 0.18 : 0.08)))
        }
        .overlay {
            if !isEnabled {
                Capsule().strokeBorder(Color.primary.opacity(0.15), style: StrokeStyle(lineWidth: 1, dash: [2, 2]))
            } else if isSecondary {
                Capsule().strokeBorder(Color.primary.opacity(0.22), lineWidth: 1)
            }
        }
        .contentShape(.capsule)
    }
}
