import CoreGraphics
import LuxtableAI
import Foundation

extension ViewerModel {
    // MARK: - Tiles (true 1:1)

    /// After zoom/pan settles, render the visible region at the zoom's scale
    /// (capped at 1:1; 200%+ magnifies 1:1 pixels) when the base frame is too coarse.
    func scheduleTile() {
        tileTask?.cancel()
        guard let request = tileRequest(), let region = request.region, let scale = request.scale,
              let app, let photo = app.focusedPhoto else { return }
        if let tile, tile.photoID == photo.id, tile.scale >= scale - 1e-6,
           let visible = visibleOrientedRegion, tile.region.contains(visible.insetBy(dx: 0.0005, dy: 0.0005)) { return }
        let edit = shownEdit(for: photo.id)
        let renderer = renderer, delay = tileDelay, gen = renderGeneration
        let developed = request.geometry == .final
        if let dev = renderer as? any GeometryPrefetching {
            // Pan slack: warm the pipeline for 3x the visible region around it (background).
            var slack = request
            slack.region = region.insetBy(dx: -region.width, dy: -region.height)
                .intersection(CGRect(x: 0, y: 0, width: 1, height: 1))
            slackTask?.cancel()
            slackTask = Task { _ = await dev.prefetch(photo, edit: edit, request: slack) }
        }
        tileTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            guard let f = try? await renderer.render(photo, edit: edit, request: request), !Task.isCancelled,
                  let self, gen == self.renderGeneration, self.app?.focusedID == photo.id else { return }
            // A frame that doesn't say which region it holds is not a tile.
            guard let got = f.region, abs(got.width - region.width) < 0.01, abs(got.height - region.height) < 0.01,
                  (f.mapping != nil || !developed) == true, developed == self.developedGeometry else {
                return
            }
            self.tile = ViewerTile(frame: f, photoID: photo.id, region: got, scale: scale)
            self.displayVersion += 1
        }
    }

    /// The region render needed for the current view, or nil when the base frame
    /// suffices or the renderer can't render regions (live preview-only mode).
    public func tileRequest() -> RenderRequest? {
        guard capabilities.regions, state.zoom != .fit, displayFrame != nil,
              let visible = visibleOrientedRegion else { return nil }
        let size = frameSpaceSize
        guard size.width > 0 else { return nil }
        let scale = min(effectiveScale, 1)
        let base = displayFrame.map { Double(max($0.width, $0.height)) / Double(max(size.width, size.height)) } ?? 0
        guard scale > base * 1.02 else { return nil }
        let margin = 0.1
        let region = visible.insetBy(dx: -visible.width * margin, dy: -visible.height * margin)
            .intersection(CGRect(x: 0, y: 0, width: 1, height: 1))
        guard !region.isEmpty else { return nil }
        let long = Int((Double(max(size.width, size.height)) * scale).rounded())
        // True 1:1 from the pipeline: a Full-quality develop of the region.
        let developed = mapping != nil
        return RenderRequest(maxPixelSize: long, scale: scale, region: region, format: renderFormat,
                             quality: capabilities.rendersGeometry ? .full : .preview, priority: .interactive,
                             geometry: developed ? .final : .oriented)
    }

    // MARK: - Zoom and pan (pane space)

    /// The pane that zoom/pan act on: the one under `point`, else the main pane.
    func pane(for point: CGPoint?) -> ViewerLayout.Pane? {
        let l = layout
        return point.flatMap(l.pane(at:)) ?? l.main
    }

    func imageSize(for pane: ViewerLayout.Pane) -> CGSize {
        guard pane.role == .pinned, let app, let p = state.comparePinned.flatMap(app.photo) else { return mainImageSize }
        return pinnedTransform().displaySize(oriented: Self.imageSize(
            meta: p.pixelSize, frame: compareFrame.map { CGSize(width: $0.width, height: $0.height) }))
    }

    /// Zoom to `mode`, keeping the image point under `anchor` (view points) fixed.
    public func zoom(to mode: ZoomMode, anchor: CGPoint? = nil) {
        guard app?.focusedPhoto != nil, let pane = pane(for: anchor) else { return }
        let image = imageSize(for: pane)
        let local = anchor.map { CGPoint(x: $0.x - pane.frame.minX, y: $0.y - pane.frame.minY) }
            ?? CGPoint(x: pane.frame.width / 2, y: pane.frame.height / 2)
        var target = mode
        if case .scale(let s) = mode {
            let clamped = min(max(s, ViewerGeometry.minScale), ViewerGeometry.maxScale)
            let fit = ViewerGeometry.deviceScale(image: image, view: pane.frame.size, backingScale: state.backingScale, zoom: .fit)
            target = clamped <= fit + 1e-6 ? .fit : .scale(clamped)
        }
        state.pan = ViewerGeometry.panForZoom(anchor: local, image: image, view: pane.frame.size,
                                              backingScale: state.backingScale, from: state.zoom, fromPan: state.pan, to: target)
        state.zoom = target
        displayVersion += 1
        scheduleTile()
    }

    /// Multiply the current zoom (scroll wheel / pinch) around `anchor`.
    public func zoom(by factor: Double, at anchor: CGPoint?) {
        guard factor.isFinite, factor > 0 else { return }
        zoom(to: .scale(effectiveScale * factor), anchor: anchor)
    }

    public func zoomIn() { step(up: true) }
    public func zoomOut() { step(up: false) }

    func step(up: Bool) {
        guard let pane = layout.main else { return }
        let fit = ViewerGeometry.deviceScale(image: mainImageSize, view: pane.frame.size, backingScale: state.backingScale, zoom: .fit)
        zoom(to: ViewerGeometry.step(from: effectiveScale, fitScale: fit, up: up), anchor: cursor)
    }

    /// Space: toggle 100% at the cursor (or the center).
    public func toggleZoomAtCursor() {
        guard app?.focusedPhoto != nil else { return }
        zoom(to: state.zoom == .fit ? .actual : .fit, anchor: cursor)
    }

    /// Pan by a drag delta; clamped to the pane under the cursor (compare panes are half width).
    public func pan(by delta: CGSize) {
        guard state.zoom != .fit, let pane = pane(for: cursor) else { return }
        let image = imageSize(for: pane)
        let s = ViewerGeometry.scale(image: image, view: pane.frame.size, backingScale: state.backingScale, zoom: state.zoom)
        let size = CGSize(width: image.width * s, height: image.height * s)
        state.pan = ViewerGeometry.clampPan(CGPoint(x: state.pan.x + delta.width, y: state.pan.y + delta.height),
                                            imageSize: size, view: pane.frame.size)
        displayVersion += 1
        scheduleTile()
    }

    /// Navigator: center the main pane on a normalized (display-space) image point.
    public func center(on u: CGPoint) {
        guard state.zoom != .fit, let pane = layout.main else { return }
        let image = mainImageSize
        let s = ViewerGeometry.scale(image: image, view: pane.frame.size, backingScale: state.backingScale, zoom: state.zoom)
        let size = CGSize(width: image.width * s, height: image.height * s)
        let raw = CGPoint(x: (0.5 - min(max(u.x, 0), 1)) * size.width, y: (0.5 - min(max(u.y, 0), 1)) * size.height)
        state.pan = ViewerGeometry.clampPan(raw, imageSize: size, view: pane.frame.size)
        displayVersion += 1
        scheduleTile()
    }

    public func setViewerSize(_ size: CGSize, backingScale: CGFloat) {
        guard size != state.viewSize || backingScale != state.backingScale else { return }
        state.viewSize = size
        state.backingScale = backingScale
        let grew = CGFloat(renderPixelSize) > CGFloat(rendered.map { max($0.width, $0.height) } ?? 0) + 64
        displayVersion += 1
        if grew { requestRender() }
    }

    // MARK: - Hover and sampling

    /// Oriented-normalized (crop space) point of the display-normalized point `u`.
    public func oriented(_ u: CGPoint) -> CGPoint {
        if let m = mapping { return m.toCrop(u) }
        return transform.toOriented(u, displayAspect: displayAspect)
    }
    /// Display-normalized point of the oriented-normalized (crop space) point `o`.
    public func displayed(_ o: CGPoint) -> CGPoint {
        if let m = mapping { return m.toFrame(o) ?? o }
        return transform.toDisplay(o, displayAspect: displayAspect)
    }

    /// Cursor moved (view points) or left (nil). Updates the readout; never
    /// touches observed state, so the Metal view isn't invalidated.
    public func hover(at p: CGPoint?) {
        cursor = p
        updatePickerLoupe(at: p)
        guard let p, let u = layout.imagePoint(at: p), let f = displayFrame else {
            cursorReadout.update(rgb: nil, at: nil)
            if loupe { onCursorRedraw?() }
            return
        }
        let o = oriented(u)
        // Developed frames are display space: sample where the cursor is.
        let s = f.mapping != nil ? u : o
        cursorReadout.update(rgb: Self.inUnit(s) && (f.mapping != nil || Self.inUnit(o)) ? ColorMath.sample(f, at: s, radius: 0) : nil,
                             at: o)
        if loupe { onCursorRedraw?() }
    }

    nonisolated static func inUnit(_ p: CGPoint) -> Bool { p.x >= 0 && p.x <= 1 && p.y >= 0 && p.y <= 1 }

    /// Average sRGB (0...1) of a 5×5 patch of the focused photo's develop render, at a
    /// normalized point of the frame (oriented, or display space for a developed final
    /// frame). Nil while only a preview is available.
    public func sampleFrame(at u: CGPoint) -> (Double, Double, Double)? {
        guard let f = frame, let c = ColorMath.sample(f, at: u) else { return nil }
        return (c.x, c.y, c.z)
    }

    /// The point of the focused photo's final (developed) frame, normalized, under the
    /// display point `u`: `u` itself in developed display; in oriented display (crop,
    /// straighten and mask tools) through crop space and a known final-frame mapping.
    /// Nil when no final-frame mapping is known yet.
    public func finalFramePoint(forDisplay u: CGPoint) -> CGPoint? {
        if frame?.mapping != nil || mapping != nil { return u }
        guard let app, let id = app.focusedID else { return nil }
        let m = mappingCache[id].flatMap { $0.key == GeometryKey(app.edit(for: id)) ? $0.mapping : nil }
        return m?.toFrame(oriented(u))
    }

    public func toggleBeforeAfter() {
        state.beforeAfter.toggle()
        tile = nil
        requestRender()
    }

    /// A drag starting at view x grabs the before/after divider (its line or handle).
    public func grabsSplit(atX x: CGFloat) -> Bool {
        layout.main?.splitX.map { abs($0 - x) < 12 } ?? false
    }

    /// Drag the before/after divider to view x (clamped to the image). The Metal surface
    /// draws the line and only redraws on `displayVersion`: setting `state.split` alone
    /// moved the SwiftUI handle but left the line where it was.
    public func dragSplit(toX x: CGFloat) {
        guard let f = layout.splitFraction(atX: x) else { return }
        state.split = f
        displayVersion += 1
    }

    public func togglePinCompare() {
        state.comparePinned = state.comparePinned == nil ? app?.focusedID : nil
        requestRender()
    }
}
