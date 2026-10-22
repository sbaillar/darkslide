import CoreGraphics
import DarkslideAI
import Foundation

extension ViewerModel {
    // MARK: - Layers

    /// Selected layer of the focused photo (nil after navigation, or if the layer is gone).
    public var selectedLayerID: Int? {
        get {
            guard let app, let id = app.focusedID, let sel = layerSelection, sel.photo == id,
                  app.edit(for: id).layers.contains(where: { $0.id == sel.layer }) else { return nil }
            return sel.layer
        }
        set {
            layerSelection = newValue.flatMap { l in app?.focusedID.map { (photo: $0, layer: l) } }
            geometryModeMayHaveChanged()
        }
    }

    /// Mask shapes of the selected layer, for the canvas overlay.
    public var selectedMask: MaskEdit? {
        guard let app, let id = app.focusedID, let lid = selectedLayerID else { return nil }
        return app.edit(for: id).layers.first { $0.id == lid }?.mask
    }

    /// Vision masks of the selected layer's AI shapes that are ready to draw (in the
    /// Metal pass). Missing ones are regenerated in the background; the view redraws when ready.
    public var selectedAIMaskKeys: [MaskOverlayCache.Key] {
        guard let id = app?.focusedID, selectedLayerID != nil, let mask = selectedMask else { return [] }
        return MaskOverlayCache.targets(in: mask).compactMap { target in
            if masks.mask(photo: id, target: target) != nil { return MaskOverlayCache.Key(photo: id, target: target) }
            masks.request(photo: id, target: target, generator: { [weak self] photo, target in
                await self?.regenerateMask(photo: photo, target: target)
            }, onReady: { [weak self] in self?.displayVersion += 1 })
            return nil
        }
    }

    /// Brush strokes to draw in the shader: the selected layer's stored strokes (oriented)
    /// and the brush/eraser drag in progress (display space, mapped to oriented).
    public var overlaySegments: [OverlaySegment] {
        var out: [OverlaySegment] = []
        if selectedLayerID != nil, let mask = selectedMask {
            for shape in mask.shapes {
                if case .brush(_, let strokes) = shape {
                    for st in strokes { out += OverlaySegment.segments(st.points, radius: Float(st.size / 2), erase: st.erase) }
                }
            }
        }
        if let d = activeDrag, d.tool == .brush || d.tool == .eraser, let pane = layout.main {
            let aspect = pane.image.width / max(pane.image.height, 1e-6)
            let pts = d.points.map { transform.toOriented($0, displayAspect: aspect) }
            out += OverlaySegment.segments(pts, radius: Float(brushSize / 2), erase: d.tool == .eraser)
        }
        out += removeOverlaySegments
        return out
    }


    // MARK: - Canvas tools

    /// Effective tool: the WB picker flag on AppModel wins.
    public var activeTool: CanvasTool {
        if app?.wbPickerActive == true { return .wbPicker }
        // The focus picker picks a depth: a tilt-shift band has none to pick.
        if let app, app.focusPickerActive, app.focusedEdit.effects.lensBlur.mode == .depth { return .focusPoint }
        return app?.colorTools.mode != nil ? .colorPicker : canvasTool // colour tools
    }

    /// Pixel size of the displayed image (crop aspect and handle tolerance).
    var displayPixelSize: CGSize { mainImageSize }

    public func beginCanvasDrag(at p: CGPoint) {
        if colorToolBegin(at: p) { return }
        guard activeTool != .pan, let pane = layout.main, let id = app?.focusedID else { return }
        let u = pane.normalized(p)
        var d = CanvasDrag(tool: activeTool, start: u)
        if activeTool == .crop {
            let tol = CGSize(width: 10 / max(pane.image.width, 1), height: 10 / max(pane.image.height, 1))
            d.cropHandle = CanvasInteraction.cropHit(u, rect: cropRect, tolerance: tol)
            d.initialCrop = cropRect
        }
        activeDrag = d
        dragPhotoID = id
    }

    public func updateCanvasDrag(to p: CGPoint) {
        if colorToolMove(to: p) { return }
        guard var d = activeDrag, let pane = layout.main else { return }
        guard dragPhotoID == app?.focusedID else {
            cancelCanvasDrag()
            return
        }
        d.move(to: pane.normalized(p))
        activeDrag = d
    }

    /// Crop rect the current crop drag would commit (overlay preview, aspect honoured).
    public var cropDragPreview: CGRect? {
        guard let d = activeDrag, d.tool == .crop, let app, let id = dragPhotoID else { return nil }
        let lock = cropLock(for: app.edit(for: id))
        let r = CanvasInteraction.cropRect(for: d, imageSize: displayPixelSize, aspect: lock.aspect, portrait: lock.portrait)
        return cropImageArea.map { CanvasInteraction.confine(r, to: $0) } ?? r
    }

    /// Finish the drag and apply its edit to the photo it started on (one undo step).
    @discardableResult
    public func endCanvasDrag() -> CanvasCommit {
        if colorToolEnd() { return .none }
        defer {
            activeDrag = nil
            dragPhotoID = nil
        }
        guard let d = activeDrag, let app, let id = dragPhotoID, id == app.focusedID else { return .none }
        let lock = cropLock(for: app.edit(for: id))
        let commit = CanvasInteraction.commit(d, imageSize: displayPixelSize, cropAspect: lock.aspect, brushSize: brushSize,
                                              cropPortrait: lock.portrait)
        apply(commit, to: id)
        app.endEditGesture()
        return commit
    }

    public func cancelCanvasDrag() {
        activeDrag = nil
        dragPhotoID = nil
    }

    /// Apply a canvas commit to photo `id` only (never the whole selection).
    func apply(_ commit: CanvasCommit, to id: PhotoID) {
        guard let app else { return }
        switch commit {
        case .none: break
        case .pickColor(let u):
            // Developed frames are display space; oriented ones are sampled in crop space.
            let o = displayFrame?.mapping != nil ? u : oriented(u)
            cursorReadout.pin(displayFrame.flatMap { Self.inUnit(o) ? ColorMath.sample($0, at: o) : nil })
        case .pickWhiteBalance(let u): pickWhiteBalance(at: u)
        case .pickFocus(let u):
            let o = oriented(u)
            app.updateEdits([id], label: "Lens Blur Focus") { _, e in
                e.effects.lensBlur.focus = LensBlurEdit.focusPoint(o)
            }
        case .straighten(let deg):
            // `deg` is the on-screen clockwise turn that levels the line, measured on the
            // displayed (already straightened) image; ds-core angles are counter-clockwise.
            app.updateEdits([id], label: "Straighten") { _, e in
                e.crop.angle = min(max(e.crop.angle - deg, -45), 45)
            }
        case .crop(let drawn):
            // Never past the image area (what the pipeline fills): no black corners.
            let r = cropImageArea.map { CanvasInteraction.confine(drawn, to: $0) } ?? drawn
            let aspect = displayAspect
            app.updateEdits([id], label: "Crop") { _, e in
                e.crop.rect = ViewTransform(crop: e.crop).canonicalRect(display: r, displayAspect: aspect)
            }
        case .mask(let shape):
            let orientedShape = transform.toOriented(shape, displayAspect: displayAspect)
                .applyingBrush(hardness: brushHardness, flow: brushFlow, autoMask: brushAutoMask)
            var layerID = selectedLayerID
            app.updateEdits([id], label: "Mask") { _, e in
                if let lid = layerID, let i = e.layers.firstIndex(where: { $0.id == lid }) {
                    e.layers[i].mask.shapes.append(orientedShape)
                } else {
                    let newID = (e.layers.map(\.id).max() ?? -1) + 1
                    e.layers.append(LayerEdit(id: newID, name: "Layer \(e.layers.count + 1)",
                                              mask: MaskEdit(shapes: [orientedShape])))
                    layerID = newID
                }
            }
            selectedLayerID = layerID
        }
        displayVersion += 1
    }

    /// Crop box of the focused photo in display space: what ds-pipeline renders (the
    /// stored rect is canonical, in oriented space; a full crop with a straighten angle
    /// shows the pipeline's auto-fit).
    public var cropRect: CGRect {
        // Developed display shows the cropped result itself: no crop box.
        guard !developedGeometry, let app, let id = app.focusedID else { return CGRect(x: 0, y: 0, width: 1, height: 1) }
        let crop = app.edit(for: id).crop
        // The pipeline's own final-frame map when planned for this edit (exact, lens and
        // keystone auto-fit included), else the angle-only auto-fit.
        return ViewTransform(crop: crop).renderedDisplayRect(crop, displayAspect: displayAspect, mapping: currentFinalMapping)
    }

    /// Quarter turns (0...3) applied to the focused photo.
    public var rotation: Int { transform.quarterTurns }

    /// Rotate 90° left (-1) or right (+1): the focused photo only.
    public func rotate(by delta: Int) {
        guard let app, let id = app.focusedID else { return }
        cancelCanvasDrag()
        app.updateEdits([id], label: delta < 0 ? "Rotate Left" : "Rotate Right") { _, e in
            e.crop.quarterTurns = CanvasInteraction.rotate(e.crop.quarterTurns, by: delta)
        }
        app.endEditGesture()
        tile = nil
        displayVersion += 1
    }

    /// Keyboard nudge for the active canvas tool: arrows move the crop by
    /// `dx`,`dy` display pixels, or straighten by 0.1° per step. Returns false when
    /// the tool doesn't take arrows (the key then navigates as usual).
    @discardableResult
    public func nudgeCanvas(dx: CGFloat, dy: CGFloat) -> Bool {
        guard let app, let id = app.focusedID else { return false }
        switch canvasTool {
        case .crop:
            nudgeCrop(dx: dx, dy: dy)
            return true
        case .straighten:
            // Right/up turns the content clockwise on screen (ds-core angle decreases).
            let step = Double(dx != 0 ? dx : -dy) * 0.1
            app.updateEdits([id], label: "Straighten", coalesceKey: "straighten-nudge") { _, e in
                e.crop.angle = min(max(e.crop.angle - step, -45), 45)
            }
            return true
        default:
            return false
        }
    }

    /// Move the crop by `dx`,`dy` display pixels.
    public func nudgeCrop(dx: CGFloat, dy: CGFloat) {
        guard let app, let id = app.focusedID else { return }
        let size = displayPixelSize
        guard size.width > 0, size.height > 0 else { return }
        let r = cropRect
        var moved = CanvasInteraction.clampRect(r.offsetBy(dx: dx / size.width, dy: dy / size.height))
        if let area = cropImageArea { moved = CanvasInteraction.confine(moved, to: area) }
        guard moved != r else { return }
        let aspect = displayAspect
        app.updateEdits([id], label: "Move Crop", coalesceKey: "crop-nudge") { _, e in
            e.crop.rect = ViewTransform(crop: e.crop).canonicalRect(display: moved, displayAspect: aspect)
        }
    }
}
