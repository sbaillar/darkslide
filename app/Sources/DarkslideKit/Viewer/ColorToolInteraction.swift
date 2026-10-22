import CoreGraphics
import Foundation

/// Canvas handling of the image tools in `AppModel.colorTools`: gradient handles, the
/// colour pickers (range, skin tone, mask colour range), the targeted adjustment drag,
/// and heal/clone spots. The viewer asks these first; they return true when they
/// consumed the event.
extension ViewerModel {
    /// The colour of the displayed image under viewer-normalized `u` (sRGB 0...1).
    func sampleDisplayed(_ u: CGPoint) -> SIMD3<Double>? {
        // What the loupe shows there (full resolution, its sample size), else the frame.
        if let s = loupeSample(display: u) { return s }
        guard let f = displayFrame else { return nil }
        let o = f.mapping != nil ? u : oriented(u)
        guard Self.inUnit(o) else { return nil }
        return ColorMath.sample(f, at: o)
    }

    /// A click or drag starts at pane point `p`.
    func colorToolBegin(at p: CGPoint) -> Bool {
        if maskHandleBegin(at: p) { return true }
        guard let app, let mode = app.colorTools.mode, let pane = layout.main, let id = app.focusedID else { return false }
        let u = pane.normalized(p)
        if mode == .remove {
            removeBegin(photo: id, at: u)
            return true
        }
        if case .spot = mode {
            app.colorTools.spotStart = (id, u)
            return true
        }
        guard let rgb = sampleDisplayed(u) else { return true }
        switch mode {
        case .pickRange:
            var index = 0
            app.updateEdits([id], label: "Add Color Range") { _, e in
                e.colorEditor.advanced.append(ColorPick.range(for: rgb))
                index = e.colorEditor.advanced.count - 1
            }
            app.colorTools.selectedRange = index
            app.colorTools.mode = nil
        case .pickSkin:
            app.updateEdits([id], label: "Pick Skin Tone") { _, e in
                e.colorEditor.skin = ColorPick.skin(for: rgb, keeping: e.colorEditor.skin)
            }
            app.colorTools.mode = nil
        case .targeted:
            app.colorTools.drag = .init(photo: id, start: u, hue: ColorPick.hueSaturation(rgb).hue, edit: app.edit(for: id))
        case .pickMaskRange:
            guard let layer = selectedLayerID else {
                app.colorTools.mode = nil
                return true
            }
            app.updateEdits([id], label: "Color Range Mask") { _, e in
                if let i = e.layers.firstIndex(where: { $0.id == layer }) {
                    e.layers[i].mask.colorRange = ColorPick.range(for: rgb)
                }
            }
            app.colorTools.mode = nil
        case .spot, .remove: break // handled above
        }
        return true
    }

    /// Heal/clone: a click adds a spot with an automatic source; a drag sets the source
    /// where the drag ends. Spots are stored in the oriented frame.
    func finishSpot(kind: SpotKind, at end: CGPoint) {
        guard let app, let start = app.colorTools.spotStart, start.photo == app.focusedID else { return }
        app.colorTools.spotStart = nil
        let radius = app.colorTools.spotRadius
        let size = orientedImageSize
        let aspect = size.height > 0 ? Double(size.width / size.height) : 1.5
        let center = oriented(start.point)
        let dragged = hypot(end.x - start.point.x, end.y - start.point.y) > 0.01
        let source: CGPoint = dragged
            ? oriented(end)
            : SpotSource.auto(center: center, radius: radius, aspect: aspect) { p, r in
                guard let f = displayFrame else { return nil }
                let at = f.mapping != nil ? displayed(p) : p
                guard Self.inUnit(at) else { return nil }
                return ColorMath.sample(f, at: at, radius: max(1, Int(r * Double(max(f.width, f.height)) / 2)))
            }
        app.updateEdits([start.photo], label: kind == .heal ? "Heal Spot" : "Clone Spot") { _, e in
            e.spots.append(SpotEdit(kind: kind, center: center, radius: radius, source: source))
        }
    }

    /// The pointer moved to pane point `p` during a colour-tool drag.
    func colorToolMove(to p: CGPoint) -> Bool {
        if maskHandleMove(to: p) { return true }
        if removeMove(to: p) { return true }
        if case .spot? = app?.colorTools.mode, let pane = layout.main {
            app?.colorTools.spotEnd = pane.normalized(p)
            return true
        }
        guard let app, case .targeted(let channel)? = app.colorTools.mode else { return false }
        guard let d = app.colorTools.drag, let pane = layout.main else { return true }
        guard d.photo == app.focusedID else {
            app.colorTools.drag = nil
            return true
        }
        let amount = TargetedAdjust.amount(from: d.start, to: pane.normalized(p))
        app.updateEdits([d.photo], label: "Targeted \(channel.rawValue)", coalesceKey: "targeted-adjust") { _, e in
            e = TargetedAdjust.apply(hue: d.hue, amount: amount, channel: channel, to: d.edit)
        }
        return true
    }

    /// The colour-tool click or drag ended (one undo step per drag).
    func colorToolEnd() -> Bool {
        if maskHandleEnd() { return true }
        if removeEnd() { return true }
        guard let app, let mode = app.colorTools.mode else { return false }
        if case .spot(let kind) = mode, let start = app.colorTools.spotStart {
            finishSpot(kind: kind, at: app.colorTools.spotEnd ?? start.point)
            app.colorTools.spotEnd = nil
            return true
        }
        if app.colorTools.drag != nil {
            app.colorTools.drag = nil
            app.endEditGesture()
        }
        return true
    }
}
