import AppKit
import Foundation
import Testing
@testable import DarkslideKit

/// Polish pass: text contrast over the design tokens.
@Suite struct PolishContrastTests {
    /// sRGB grey level of a system colour in an appearance, composited over `background`
    /// (label colours are translucent).
    @MainActor static func resolved(_ color: NSColor, _ appearance: NSAppearance.Name, over background: Double = 0) -> Double {
        var out = 0.0
        NSAppearance(named: appearance)!.performAsCurrentDrawingAppearance {
            let c = color.usingColorSpace(.sRGB)!
            let grey = 0.2126 * c.redComponent + 0.7152 * c.greenComponent + 0.0722 * c.blueComponent
            out = grey * c.alphaComponent + background * (1 - c.alphaComponent)
        }
        return out
    }

    /// Sheets and popovers sit on the system window background: the palette's stand-in
    /// for it is right, and every text token clears 4.5:1 on the real thing.
    @MainActor @Test func textTokensMeetAAOnTheSystemWindowBackground() {
        for (name, palette, appearance) in [("dark", GreyPalette.dark, NSAppearance.Name.darkAqua),
                                            ("light", GreyPalette.light, .aqua)] {
            let window = Self.resolved(.windowBackgroundColor, appearance)
            #expect(abs(window - palette.window) < 0.03, "\(name) window \(window) vs \(palette.window)")
            for text in [palette.textPrimary, palette.textSecondary, palette.textTertiary] {
                let r = Contrast.ratio(grey: window, grey: text)
                #expect(r >= 4.5, "\(name) \(text) on window \(window): \(r)")
            }
        }
    }

    /// Why the views use the tokens: the system's translucent secondary and tertiary
    /// labels fall under 4.5:1 on our greys (light mode and the viewer surround worst).
    @MainActor @Test func systemSecondaryLabelsFallShortWhereTheTokensDont() {
        let light = GreyPalette.light
        for bg in [light.panel, light.viewer] {
            let system = Self.resolved(.secondaryLabelColor, .aqua, over: bg)
            #expect(Contrast.ratio(grey: bg, grey: system) < 4.5, "system secondary on \(bg)")
            #expect(Contrast.ratio(grey: bg, grey: light.textSecondary) >= 4.5, "token on \(bg)")
        }
        let dark = GreyPalette.dark
        let tertiary = Self.resolved(.tertiaryLabelColor, .darkAqua, over: dark.panel)
        #expect(Contrast.ratio(grey: dark.panel, grey: tertiary) < 3)
        #expect(Contrast.ratio(grey: dark.panel, grey: dark.textTertiary) >= 4.5)
    }

    /// Section headers and secondary labels use the tested tokens, never the system's
    /// translucent `.secondary` / `.tertiary` styles (which measured about 3:1).
    @Test func viewsUseTheContrastTokensForSecondaryText() throws {
        let views = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../Sources/DarkslideKit/Views").standardized
        let files = try #require(FileManager.default.enumerator(at: views, includingPropertiesForKeys: nil))
            .compactMap { $0 as? URL }.filter { $0.pathExtension == "swift" }
        #expect(files.count > 20)
        let banned = try NSRegularExpression(pattern: #"foregroundStyle\(\.(secondary|tertiary|quaternary)\)|: \.secondary\)|Color\.secondary\b"#)
        for f in files {
            let text = try String(contentsOf: f, encoding: .utf8)
            let lines = text.components(separatedBy: "\n").enumerated().filter { _, line in
                banned.firstMatch(in: line, range: NSRange(line.startIndex..., in: line)) != nil
            }
            #expect(lines.isEmpty, "\(f.lastPathComponent): \(lines.map { "\($0.offset + 1): \($0.element)" })")
        }
    }
}
