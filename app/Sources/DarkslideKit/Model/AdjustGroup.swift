import Foundation

/// Copyable adjustment groups, in `ds_core::AdjustGroup::ALL` order.
/// `DarkslideKitTests` checks this list against the Rust `adjustGroups()` export.
public enum AdjustGroup: String, CaseIterable, Sendable, Codable, Identifiable {
    case profile, whiteBalance, exposure, presence, hdr, levels, curve, colorBalance, colorEditor
    case detail, noise, vignette, effects, lens, crop, layers, spots

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .profile: "Camera Profile"
        case .whiteBalance: "White Balance"
        case .exposure: "Exposure"
        case .presence: "Presence"
        case .hdr: "High Dynamic Range"
        case .levels: "Levels"
        case .curve: "Curve"
        case .colorBalance: "Color Balance"
        case .colorEditor: "Color Editor"
        case .detail: "Clarity & Sharpening"
        case .noise: "Noise Reduction"
        case .vignette: "Vignetting"
        case .effects: "Effects"
        case .lens: "Lens Correction"
        case .crop: "Crop & Straighten"
        case .layers: "Layers"
        case .spots: "Heal & Clone"
        }
    }

    public var symbol: String {
        switch self {
        case .profile: "camera.filters"
        case .whiteBalance: "thermometer.sun"
        case .exposure: "plusminus.circle"
        case .presence: "sparkles"
        case .hdr: "circle.lefthalf.filled"
        case .levels: "chart.bar.xaxis"
        case .curve: "point.topleft.down.to.point.bottomright.curvepath"
        case .colorBalance: "circle.hexagongrid"
        case .colorEditor: "paintpalette"
        case .detail: "triangle"
        case .noise: "aqi.medium"
        case .vignette: "circle.dashed"
        case .effects: "wand.and.stars"
        case .lens: "camera.aperture"
        case .crop: "crop"
        case .layers: "square.3.layers.3d"
        case .spots: "bandage"
        }
    }

    /// Relative (delta) paste is meaningful only for these (matches ds-core).
    public var supportsRelative: Bool {
        switch self {
        case .whiteBalance, .exposure, .presence, .hdr: true
        default: false
        }
    }

    /// Per-photo groups Copy/Apply never preselects (N6): the user ticks them.
    public var isOptIn: Bool {
        switch self {
        case .crop, .layers, .spots: true
        default: false
        }
    }

    /// What Copy takes until the user ticks otherwise: everything but the opt-in groups.
    public static let copiedByDefault = Set(allCases.filter { !$0.isOptIn })

    /// Whether `a` and `b` differ in this group.
    public func differs(_ a: EditState, _ b: EditState) -> Bool {
        switch self {
        case .profile: a.profile != b.profile
        case .whiteBalance: a.whiteBalance != b.whiteBalance
        case .exposure: a.exposure != b.exposure
        case .presence: a.presence != b.presence
        case .hdr: a.hdr != b.hdr
        case .levels: a.levels != b.levels
        case .curve: a.curve != b.curve
        case .colorBalance: a.colorBalance != b.colorBalance
        case .colorEditor: a.colorEditor != b.colorEditor
        case .detail: a.detail != b.detail
        case .noise: a.noise != b.noise
        case .vignette: a.vignette != b.vignette
        case .effects: a.effects != b.effects
        case .lens: a.lens != b.lens
        case .crop: a.crop != b.crop
        case .layers: a.layers != b.layers
        case .spots: a.spots != b.spots || a.removals != b.removals
        }
    }

    /// Groups where `edit` differs from the identity (the "only edited" default).
    public static func changed(in edit: EditState) -> Set<AdjustGroup> {
        Set(allCases.filter { $0.differs(edit, EditState()) })
    }
}

/// What Copy/Apply pastes and how. Mirrors `ds_core::PasteOptions`.
public struct PasteOptions: Equatable, Sendable {
    public var groups: Set<AdjustGroup>
    /// Groups pasted relatively; ignored for groups that don't support it.
    public var relative: Set<AdjustGroup>
    /// Layer ids to paste (empty = all layers).
    public var layerIDs: Set<Int>
    /// A pasted Crop & Straighten keeps each target's own straighten angle (the batch
    /// gets the crop, every frame keeps its horizon). Swift-only for now: ds-core's
    /// `PasteOptions` gets it when the session facade lands.
    public var keepStraighten = false

    public init(groups: Set<AdjustGroup> = Set(AdjustGroup.allCases), relative: Set<AdjustGroup> = [], layerIDs: Set<Int> = []) {
        self.groups = groups
        self.relative = relative
        self.layerIDs = layerIDs
    }

    func isRelative(_ g: AdjustGroup) -> Bool { g.supportsRelative && relative.contains(g) }
}

/// A copied edit. Swift stand-in for `ds_core::Clipboard::apply` until the
/// session facade lands; relative groups transfer `source - identity`.
public struct AdjustmentClipboard: Equatable, Sendable {
    public var source: EditState
    public var sourceID: PhotoID?
    /// Captured at Copy (photo ids are per session, so Apply never looks the source up
    /// again): its file name, oriented pixel size and as-shot white balance.
    public var sourceName: String?
    public var sourceSize: CGSize?
    public var sourceAsShot: WhiteBalanceValue?
    /// A tool panel header's Copy: only this group is pasted, until the next Copy.
    public var onlyGroup: AdjustGroup?

    public init(source: EditState, sourceID: PhotoID? = nil, sourceName: String? = nil, sourceSize: CGSize? = nil,
                sourceAsShot: WhiteBalanceValue? = nil, onlyGroup: AdjustGroup? = nil) {
        self.source = source
        self.sourceID = sourceID
        self.sourceName = sourceName
        self.sourceSize = sourceSize
        self.sourceAsShot = sourceAsShot
        self.onlyGroup = onlyGroup
    }

    /// A relative white balance onto a target with `target` WB can't read As Shot as
    /// numbers, so `relativeWhiteBalance` pastes the source's value as is.
    static func relativeWhiteBalanceFallsBack(source: WhiteBalanceEdit, target: WhiteBalanceEdit,
                                              sourceAsShot: WhiteBalanceValue?, targetAsShot: WhiteBalanceValue?) -> Bool {
        guard !source.asShot else { return false }
        return sourceAsShot == nil || (target.asShot && targetAsShot == nil)
    }

    /// `sourceAsShot`/`targetAsShot`: each side's as-shot white balance, so a relative WB
    /// paste can read `As Shot` as numbers (ds-core `PasteTarget.as_shot`): the target
    /// gets `target K × source K / source as-shot K` and `target tint + (source − as-shot)`.
    /// Without them it falls back to the source's absolute value.
    /// ds-core `Clipboard::relative_wb`: Kelvin transfers as a ratio to the source's own
    /// as-shot (the baseline), tint as a difference. An unchanged source (still As Shot)
    /// leaves the target alone; a missing as-shot value pastes the source absolutely.
    static func relativeWhiteBalance(source: WhiteBalanceEdit, target: WhiteBalanceEdit,
                                     sourceAsShot: WhiteBalanceValue?, targetAsShot: WhiteBalanceValue?) -> WhiteBalanceEdit {
        if source.asShot { return target }
        func resolve(_ wb: WhiteBalanceEdit, _ asShot: WhiteBalanceValue?) -> WhiteBalanceValue? {
            wb.asShot ? asShot : WhiteBalanceValue(kelvin: wb.temperature, tint: wb.tint)
        }
        guard let b = sourceAsShot, let s = resolve(source, sourceAsShot), let t = resolve(target, targetAsShot),
              b.kelvin > 0 else { return source }
        return .custom(WhiteBalanceValue(kelvin: (t.kelvin * s.kelvin / b.kelvin).clamped(2000, 50000),
                                         tint: (t.tint + s.tint - b.tint).clamped(-150, 150)))
    }

    /// `sourceSize`/`targetSize`: each photo's oriented pixel size, so a crop lands as
    /// ds-core `adapt_crop` says (M10: a 4:5 crop stays 4:5 in the target's pixels, across
    /// portrait and landscape). Without them the crop is pasted as is.
    public func apply(to target: EditState, options: PasteOptions, sourceAsShot: WhiteBalanceValue? = nil,
                      targetAsShot: WhiteBalanceValue? = nil, sourceSize: CGSize? = nil, targetSize: CGSize? = nil) -> EditState {
        var out = target
        for g in AdjustGroup.allCases where options.groups.contains(g) {
            let rel = options.isRelative(g)
            switch g {
            case .whiteBalance:
                if rel {
                    out.whiteBalance = Self.relativeWhiteBalance(source: source.whiteBalance, target: target.whiteBalance,
                                                                 sourceAsShot: sourceAsShot, targetAsShot: targetAsShot)
                } else {
                    out.whiteBalance = source.whiteBalance
                }
            case .exposure:
                if rel {
                    out.exposure.ev = (target.exposure.ev + source.exposure.ev).clamped(-5, 5)
                    out.exposure.contrast = (target.exposure.contrast + source.exposure.contrast).clamped(-100, 100)
                    out.exposure.brightness = (target.exposure.brightness + source.exposure.brightness).clamped(-100, 100)
                    out.exposure.saturation = (target.exposure.saturation + source.exposure.saturation).clamped(-100, 100)
                } else {
                    out.exposure = source.exposure
                }
            case .presence:
                if rel {
                    out.presence.vibrance = (target.presence.vibrance + source.presence.vibrance).clamped(-100, 100)
                    out.presence.texture = (target.presence.texture + source.presence.texture).clamped(-100, 100)
                    out.presence.dehaze = (target.presence.dehaze + source.presence.dehaze).clamped(-100, 100)
                } else {
                    out.presence = source.presence
                }
            case .profile: out.profile = source.profile
            case .spots:
                out.spots = source.spots
                out.removals = source.removals
            case .hdr:
                if rel {
                    out.hdr.highlights = (target.hdr.highlights + source.hdr.highlights).clamped(-100, 100)
                    out.hdr.shadows = (target.hdr.shadows + source.hdr.shadows).clamped(-100, 100)
                    out.hdr.whites = (target.hdr.whites + source.hdr.whites).clamped(-100, 100)
                    out.hdr.blacks = (target.hdr.blacks + source.hdr.blacks).clamped(-100, 100)
                } else {
                    out.hdr = source.hdr
                }
            case .levels: out.levels = source.levels
            case .curve: out.curve = source.curve
            case .colorBalance: out.colorBalance = source.colorBalance
            case .colorEditor: out.colorEditor = source.colorEditor
            case .detail: out.detail = source.detail
            case .noise: out.noise = source.noise
            case .vignette: out.vignette = source.vignette
            case .effects: out.effects = source.effects
            case .lens: out.lens = source.lens
            case .crop:
                // L7: the rotation belongs to each photo; the crop follows the target as shown.
                if let sourceSize, let targetSize {
                    out.crop = CropPaste.adapt(source.crop, from: sourceSize, to: targetSize, targetTurns: target.crop.quarterTurns)
                } else {
                    out.crop = source.crop
                    out.crop.quarterTurns = target.crop.quarterTurns
                }
                if options.keepStraighten { out.crop.angle = target.crop.angle }
                // A rect from another photo (or kept with another angle) must not show empty corners.
                if let targetSize { out.crop = CropPaste.inscribe(out.crop, in: targetSize) }
            case .layers:
                let picked = source.layers.filter { options.layerIDs.isEmpty || options.layerIDs.contains($0.id) }
                var nextID = (out.layers.map(\.id).max() ?? -1) + 1
                for var layer in picked {
                    layer.id = nextID
                    nextID += 1
                    out.layers.append(layer)
                }
            }
        }
        // A group pasted as is (or reset) replaces what a look gave it; a relative one
        // adds to it, so the look still owns its share.
        let replaced = options.groups.filter { !options.isRelative($0) }
        out.look = target.look?.dropping { EditCodec.group(of: $0).map(replaced.contains) ?? false }
        return out
    }
}

extension Comparable {
    func clamped(_ lo: Self, _ hi: Self) -> Self { min(max(self, lo), hi) }
}
