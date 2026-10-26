import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Luxtable's own look: faint warmth on the chrome only, the clipboard chip, the
/// familiar-layout status line.
@MainActor
@Suite struct LuxtableLookTests {
    @Test func chromeWarmthIsFaintAndKeepsContrast() {
        for (tint, palette) in [(ChromeTint.dark, GreyPalette.dark), (ChromeTint.light, GreyPalette.light)] {
            #expect(tint.maxOffset <= 0.02, "faint: never a colour cast")
            #expect(tint.red > 0 && tint.blue < 0, "warm: amber, not blue")
            for bg in [palette.panel, palette.chrome] {
                let c = tint.apply(bg)
                let lb = Contrast.luminance(r: c.r, g: c.g, b: c.b)
                for fg in [palette.textPrimary, palette.textSecondary, palette.textTertiary] {
                    #expect(Contrast.ratio(lb, Contrast.luminance(fg)) >= 4.5, "text stays readable on tinted \(bg)")
                }
            }
        }
        let clamped = ChromeTint(red: 0.5, green: 0, blue: -0.5).apply(0.9)
        #expect(clamped.r == 1 && clamped.b == 0.4)
    }

    @Test func clipboardChipNamesWhatIsCopied() async {
        let m = makeModel()
        await m.start()
        #expect(m.clipboardChip == nil, "nothing copied yet")
        m.updateEdit(label: "setup") {
            $0.exposure.ev = 0.5
            $0.hdr.shadows = 20
            $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 6400, tint: 2))
        }
        m.perform(.copyAdjustments)
        let chip = m.clipboardChip
        #expect(chip != nil)
        #expect(chip?.contains("+") == true || chip?.contains(" · ") == true)
        #expect((chip?.count ?? 99) < 40, "short enough for the toolbar")
    }

    @Test func statusLine() {
        #expect(FamiliarText.statusLine(.luxtable, .luxtable) == "Luxtable layout and keys")
        #expect(FamiliarText.statusLine(.captureOne, .lightroomClassic) == "Capture One layout · Lightroom Classic keys")
    }
}
