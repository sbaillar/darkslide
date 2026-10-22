import Foundation
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// The built-in looks and styles exactly as Swift code had them before they moved into
/// the bundled `darkslide.looks` plug-in: the reference the pack must reproduce field for
/// field (`PluginTests`).
enum LegacyBuiltIns {
    static let categories = ["Black & White", "Film", "Color", "Tone", "Detail"]

    static let styles: [Preset] = {
        var list: [Preset] = []
        func add(_ n: Int, _ name: String, _ category: String, _ groups: Set<AdjustGroup>,
                 exact: Set<String> = [], _ change: (inout EditState) -> Void) {
            var e = EditState()
            change(&e)
            let id = UUID(uuidString: String(format: "00000000-0000-0000-0000-00000000A%03X", n))!
            list.append(Preset(id: id, name: name, groups: groups, relative: [.exposure, .hdr], absoluteFields: exact,
                               edit: e, isBuiltIn: true, category: category))
        }
        func curve(_ pts: [(Double, Double)]) -> [CurvePoint] { pts.map { CurvePoint($0.0, $0.1) } }
        let mono: Set<String> = ["exposure.saturation"]

        // Black & White: saturation is set exactly (−100), whatever the photo had.
        add(0x010, "B&W Neutral", "Black & White", [.exposure], exact: mono) { e in
            e.exposure.saturation = -100
        }
        add(0x004, "B&W High Contrast", "Black & White", [.exposure, .curve], exact: mono) { e in
            e.exposure.saturation = -100
            e.exposure.contrast = 35
            e.curve.points[.luma] = curve([(0.25, 0.18), (0.75, 0.84)])
        }
        add(0x011, "B&W Soft Matte", "Black & White", [.exposure, .hdr, .curve], exact: mono) { e in
            e.exposure.saturation = -100
            e.exposure.contrast = -12
            e.hdr.blacks = 10
            e.curve.points[.luma] = curve([(0.0, 0.08), (0.5, 0.5), (1.0, 0.95)])
        }
        add(0x012, "B&W Selenium", "Black & White", [.exposure, .curve, .colorBalance], exact: mono) { e in
            e.exposure.saturation = -100
            e.exposure.contrast = 15
            e.curve.points[.luma] = curve([(0.2, 0.17), (0.8, 0.83)])
            e.colorBalance.shadows = WheelValue(hue: 265, saturation: 0.10, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 35, saturation: 0.05, lightness: 0)
        }
        add(0x013, "B&W Infrared Glow", "Black & White", [.exposure, .hdr, .colorEditor], exact: mono) { e in
            e.exposure.saturation = -100
            e.hdr.highlights = 15
            e.colorEditor.bands[3].lightness = 60 // green foliage turns bright
            e.colorEditor.bands[2].lightness = 30
            e.colorEditor.bands[5].lightness = -40 // blue sky goes dark
        }

        // Film looks.
        add(0x002, "Warm Film", "Film", [.exposure, .hdr, .curve, .colorBalance]) { e in
            e.exposure.contrast = -8
            e.exposure.saturation = -10
            e.hdr.blacks = 12
            e.curve.points[.rgb] = curve([(0.0, 0.05), (0.5, 0.52), (1.0, 0.96)])
            e.colorBalance.shadows = WheelValue(hue: 200, saturation: 0.12, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 40, saturation: 0.18, lightness: 0)
        }
        add(0x020, "Portrait Film", "Film", [.exposure, .curve, .colorBalance, .colorEditor]) { e in
            e.exposure.contrast = -6
            e.exposure.saturation = -8
            e.curve.points[.rgb] = curve([(0.0, 0.03), (0.3, 0.29), (0.7, 0.72), (1.0, 0.97)])
            e.colorBalance.highlights = WheelValue(hue: 35, saturation: 0.08, lightness: 0)
            e.colorEditor.bands[1].saturation = -8 // calmer skin
            e.colorEditor.bands[1].lightness = 6
        }
        add(0x021, "Chrome", "Film", [.exposure, .curve, .colorBalance, .colorEditor]) { e in
            e.exposure.contrast = 12
            e.exposure.saturation = -18
            e.curve.points[.rgb] = curve([(0.25, 0.22), (0.75, 0.78)])
            e.colorBalance.shadows = WheelValue(hue: 190, saturation: 0.06, lightness: 0)
            e.colorEditor.bands[5].saturation = -10
            e.colorEditor.bands[5].lightness = -8
        }
        add(0x022, "Faded Film", "Film", [.exposure, .hdr, .curve, .colorBalance]) { e in
            e.exposure.saturation = -15
            e.hdr.blacks = 20
            e.curve.points[.rgb] = curve([(0.0, 0.10), (0.5, 0.50), (1.0, 0.92)])
            e.colorBalance.shadows = WheelValue(hue: 215, saturation: 0.08, lightness: 0)
        }
        add(0x023, "Slide Film", "Film", [.exposure, .curve, .colorEditor]) { e in
            e.exposure.contrast = 20
            e.exposure.saturation = 18
            e.curve.points[.rgb] = curve([(0.2, 0.15), (0.8, 0.86)])
            e.colorEditor.bands[5].saturation = 12
            e.colorEditor.bands[3].saturation = 8
        }

        // Colour.
        add(0x030, "Warm", "Color", [.colorBalance]) { e in
            e.colorBalance.master = WheelValue(hue: 40, saturation: 0.10, lightness: 0)
        }
        add(0x031, "Cool", "Color", [.colorBalance]) { e in
            e.colorBalance.master = WheelValue(hue: 210, saturation: 0.10, lightness: 0)
        }
        add(0x032, "Teal & Orange", "Color", [.colorBalance, .colorEditor]) { e in
            e.colorBalance.shadows = WheelValue(hue: 190, saturation: 0.15, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 35, saturation: 0.15, lightness: 0)
            e.colorEditor.bands[1].saturation = 10
            e.colorEditor.bands[5].hueShift = -10
        }
        add(0x033, "Golden Hour", "Color", [.exposure, .hdr, .colorBalance]) { e in
            e.exposure.saturation = 8
            e.hdr.highlights = -10
            e.colorBalance.midtones = WheelValue(hue: 38, saturation: 0.10, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 45, saturation: 0.12, lightness: 0)
        }
        add(0x003, "Punchy", "Color", [.exposure, .hdr, .detail]) { e in
            e.exposure.contrast = 22
            e.exposure.saturation = 15
            e.hdr.highlights = -20
            e.hdr.shadows = 15
            e.detail.clarity = 20
        }
        add(0x034, "Vivid", "Color", [.exposure, .detail]) { e in
            e.exposure.saturation = 25
            e.exposure.contrast = 12
            e.detail.clarity = 10
        }

        // Tone.
        add(0x040, "High-Key", "Tone", [.exposure, .hdr]) { e in
            e.exposure.ev = 0.5
            e.exposure.contrast = -15
            e.hdr.highlights = -15
            e.hdr.shadows = 30
            e.hdr.blacks = 10
        }
        add(0x041, "Low-Key", "Tone", [.exposure, .hdr]) { e in
            e.exposure.ev = -0.4
            e.exposure.contrast = 20
            e.hdr.highlights = -20
            e.hdr.blacks = -15
        }
        add(0x042, "Matte", "Tone", [.exposure, .hdr, .curve]) { e in
            e.exposure.contrast = -10
            e.hdr.blacks = 15
            e.curve.points[.rgb] = curve([(0.0, 0.08), (0.25, 0.27), (1.0, 1.0)])
        }

        // Detail.
        add(0x050, "Landscape Clarity", "Detail", [.exposure, .hdr, .detail]) { e in
            e.exposure.saturation = 10
            e.hdr.highlights = -25
            e.hdr.shadows = 20
            e.detail.clarity = 25
            e.detail.structure = 15
        }
        return list
    }()

    static let looks: [Look] = {
        var list: [Look] = []
        func add(_ n: Int, _ id: String, _ name: String, _ summary: String, exact: Set<String> = [],
                 _ change: (inout EditState) -> Void) {
            var e = EditState()
            change(&e)
            let groups = AdjustGroup.changed(in: e)
            let uuid = UUID(uuidString: String(format: "00000000-0000-0000-0000-00000000B%03X", n))!
            let p = Preset(id: uuid, name: name, groups: groups, relative: [.exposure, .presence, .hdr],
                           absoluteFields: exact.union(LookPack.effectFields(e)), edit: e, isBuiltIn: true, category: LookPack.category)
            list.append(Look(id: "look." + id, name: name, summary: summary, preset: p))
        }
        let mono: Set<String> = ["exposure.saturation"]
        add(0x01, "portrait", "Portrait", "Background softened by depth, gentle skin tones") { e in
            e.effects.lensBlur.amount = 45
            e.effects.lensBlur.focalRange = 25
            e.effects.lensBlur.highlightBoost = 30
            e.exposure.contrast = -5
            e.hdr.shadows = 8
            e.presence.vibrance = 6
            e.detail.clarity = -8
            e.colorEditor.bands[1].saturation = -6 // calmer skin
            e.colorEditor.bands[1].lightness = 5
        }
        add(0x02, "cinematic", "Cinematic", "Teal shadows, warm highlights, soft contrast") { e in
            e.colorBalance.shadows = WheelValue(hue: 190, saturation: 0.16, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 35, saturation: 0.14, lightness: 0)
            e.exposure.contrast = 10
            e.exposure.saturation = -8
            e.hdr.highlights = -15
            e.hdr.blacks = 6
            e.colorEditor.bands[5].hueShift = -8
            e.vignette.amountEV = -0.3
        }
        add(0x03, "warm-negative", "Warm Negative", "Warm colour-negative film: gentle contrast, lifted blacks, grain") { e in
            e.colorBalance.master = WheelValue(hue: 40, saturation: 0.08, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 45, saturation: 0.10, lightness: 0)
            e.exposure.contrast = -8
            e.exposure.saturation = -6
            e.hdr.blacks = 12
            e.levels.outBlack = 0.03
            e.effects.grain.amount = 12
        }
        add(0x04, "cool-slide", "Cool Slide", "Crisp slide film: deep contrast, cool shadows, rich blues") { e in
            e.exposure.contrast = 18
            e.exposure.saturation = 12
            e.colorBalance.shadows = WheelValue(hue: 220, saturation: 0.10, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 200, saturation: 0.04, lightness: 0)
            e.colorEditor.bands[5].saturation = 10
        }
        add(0x05, "faded-matte", "Faded Matte", "Milky blacks, softer whites, muted colour") { e in
            e.levels.outBlack = 0.08
            e.levels.outWhite = 0.95
            e.exposure.contrast = -12
            e.exposure.saturation = -15
            e.colorBalance.shadows = WheelValue(hue: 215, saturation: 0.06, lightness: 0)
        }
        add(0x06, "cross-processed", "Cross-Processed", "Blue shadows, yellow-green highlights, punchy") { e in
            e.colorBalance.shadows = WheelValue(hue: 230, saturation: 0.18, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 60, saturation: 0.18, lightness: 0)
            e.colorBalance.midtones = WheelValue(hue: 120, saturation: 0.05, lightness: 0)
            e.exposure.contrast = 20
            e.exposure.saturation = 15
        }
        add(0x07, "vintage", "Vintage", "Faded, warm and grainy with a slight vignette") { e in
            e.levels.outBlack = 0.06
            e.exposure.saturation = -20
            e.exposure.contrast = -6
            e.colorBalance.master = WheelValue(hue: 35, saturation: 0.12, lightness: 0)
            e.effects.grain.amount = 30
            e.effects.grain.size = 40
            e.vignette.amountEV = -0.5
        }
        add(0x08, "bw-high-contrast", "B&W High Contrast", "Black and white with deep blacks and bright whites", exact: mono) { e in
            e.exposure.saturation = -100
            e.exposure.contrast = 35
            e.hdr.blacks = -10
            e.hdr.whites = 10
            e.detail.clarity = 10
        }
        add(0x09, "bw-soft", "B&W Soft", "Gentle black and white with a soft glow", exact: mono) { e in
            e.exposure.saturation = -100
            e.exposure.contrast = -12
            e.hdr.blacks = 10
            e.effects.glow.amount = 15
            e.effects.glow.size = 40
        }
        add(0x0A, "noir", "Noir", "Hard black and white, heavy vignette and grain", exact: mono) { e in
            e.exposure.saturation = -100
            e.exposure.contrast = 40
            e.exposure.ev = -0.3
            e.hdr.blacks = -15
            e.vignette.amountEV = -1.2
            e.vignette.midpoint = 0.35
            e.effects.grain.amount = 35
            e.effects.grain.size = 35
            e.effects.grain.roughness = 60
        }
        add(0x0B, "moody", "Moody", "Darker, muted and cool, highlights held back") { e in
            e.exposure.ev = -0.35
            e.exposure.contrast = 12
            e.exposure.saturation = -18
            e.hdr.highlights = -25
            e.hdr.blacks = -8
            e.colorBalance.shadows = WheelValue(hue: 210, saturation: 0.08, lightness: 0)
            e.vignette.amountEV = -0.4
            e.detail.clarity = 6
        }
        add(0x0C, "bright-airy", "Bright & Airy", "Lighter and softer, open shadows, clean colour") { e in
            e.exposure.ev = 0.35
            e.exposure.contrast = -15
            e.exposure.saturation = -6
            e.hdr.shadows = 25
            e.hdr.highlights = -10
            e.presence.vibrance = 10
            e.levels.outBlack = 0.02
        }
        add(0x0D, "golden-hour", "Golden Hour", "Warm, glowing late-day light") { e in
            e.colorBalance.midtones = WheelValue(hue: 38, saturation: 0.12, lightness: 0)
            e.colorBalance.highlights = WheelValue(hue: 45, saturation: 0.14, lightness: 0)
            e.exposure.saturation = 8
            e.presence.vibrance = 10
            e.hdr.highlights = -10
            e.effects.glow.amount = 12
            e.effects.glow.size = 60
        }
        add(0x0E, "dreamy-glow", "Dreamy Glow", "A soft glow over the highlights, gentle contrast") { e in
            e.effects.glow.amount = 45
            e.effects.glow.size = 60
            e.exposure.contrast = -10
            e.exposure.saturation = -5
            e.hdr.highlights = -10
            e.detail.clarity = -15
        }
        add(0x0F, "pop", "Pop", "Vivid colour and crisp contrast") { e in
            e.exposure.saturation = 20
            e.presence.vibrance = 20
            e.exposure.contrast = 15
            e.detail.clarity = 12
        }
        add(0x10, "hdr-detail", "HDR Detail", "Bright skies held, shadows opened, local detail") { e in
            e.hdr.highlights = -40
            e.hdr.shadows = 40
            e.detail.clarity = 25
            e.detail.structure = 20
            e.presence.vibrance = 10
        }
        add(0x11, "tilt-shift", "Tilt-Shift Miniature", "A sharp band, blurred above and below: a model-sized world") { e in
            e.effects.lensBlur.amount = 60
            e.effects.lensBlur.mode = .plane
            e.effects.lensBlur.highlightBoost = 20
            e.exposure.saturation = 20
            e.exposure.contrast = 15
        }
        add(0x12, "halation-film", "Halation Film", "Red glow around highlights, fine grain") { e in
            e.effects.halation.amount = 45
            e.effects.grain.amount = 20
            e.exposure.contrast = 6
            e.colorBalance.highlights = WheelValue(hue: 30, saturation: 0.06, lightness: 0)
        }
        return list
    }()
}
