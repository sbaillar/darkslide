import CoreGraphics
import LuxtableAI
import Foundation
import Observation

/// Viewer presentation state (the math lives in `ViewerGeometry`).
public struct ViewerState: Equatable, Sendable {
    public var zoom: ZoomMode = .fit
    /// Offset of the image center from the pane center (pane-local points).
    public var pan: CGPoint = .zero
    public var beforeAfter = false
    /// 0...1 across the pane.
    public var split: Double = 0.5
    /// Image pinned on the left in compare mode.
    public var comparePinned: PhotoID?
    /// Pane size in points and its backing scale, reported by the view.
    public var viewSize: CGSize = .zero
    public var backingScale: CGFloat = 2
    public var fullScreen = false

    public init() {}
}

extension Photo {
    /// Pixel size from metadata (oriented).
    var pixelSize: CGSize { CGSize(width: meta.pixelWidth, height: meta.pixelHeight) }
}

/// A 1:1 (or scale ≤ 1) render of part of the image, drawn over the fit frame.
public struct ViewerTile: Equatable, Sendable {
    public var frame: RenderedFrame
    public var photoID: PhotoID
    /// Normalized region of the (oriented) image the tile covers.
    public var region: CGRect
    /// Device pixels per image pixel the tile was rendered at.
    public var scale: Double
}

/// Viewer model: presentation state, render scheduling, and the frames on screen.
/// Reads the focused photo and its edit from `app`; `AppModel` notifies it via
/// `focusChanged()` / `editsChanged(_:)`.
///
/// Frames shown always belong to the focused photo: on navigation the embedded
/// preview appears first (prefetched for the neighbours), stale renders are
/// cancelled, and the develop render replaces the preview when it lands.
///
/// Canvas coordinates are **display** space (after quarter turns and straighten,
/// see `ViewTransform`); frames, tiles, masks and color samples are **oriented**
/// space. Every conversion goes through `transform`.
@MainActor @Observable
public final class ViewerModel {
    @ObservationIgnored weak var app: AppModel?
    @ObservationIgnored let renderer: any Renderer
    @ObservationIgnored let previews: (any PreviewProvider)?
    @ObservationIgnored let previewCache: ViewerPreviewCache?

    /// Previews prefetched ahead while an arrow is held.
    @ObservationIgnored public var fastPrefetchAhead = 8
    @ObservationIgnored var lastPrefetchIndex: Int?
    @ObservationIgnored var navigatorCache: (key: String, image: CGImage)?
    /// The key of a navigator image being made off the main thread (float frames).
    @ObservationIgnored var navigatorPending: String?
    /// Bumped when an off-main navigator image lands (the view reads it again).
    var navigatorGeneration = 0
    /// What the renderer can do (regions, float frames, edits).
    @ObservationIgnored public let capabilities: RendererCapabilities

    public var state = ViewerState()
    /// Develop render of `renderedID` (not necessarily the focused photo).
    var rendered: RenderedFrame?
    var renderedID: PhotoID?
    /// Embedded preview of the focused photo, shown until the render lands.
    public internal(set) var previewFrame: RenderedFrame?
    public internal(set) var previewID: PhotoID?
    public internal(set) var tile: ViewerTile?
    public private(set) var beforeFrame: RenderedFrame?
    public private(set) var compareFrame: RenderedFrame?
    public private(set) var histogram: Histogram?
    public private(set) var renderError: String?
    /// The last develop render of the focused photo failed for a reason other than a
    /// missing original (GPU, decode): shown with Retry; cleared by a good render.
    public private(set) var renderFailure: String?
    /// `renderFailure` says the file can't be developed at all (an unsupported RAW
    /// format or sensor), so the banner offers no Retry.
    private var renderFailureIsUnsupported = false
    /// Bumped when an edit is made to a photo that can't be previewed (its original is
    /// missing): the banner pulses, so a slider still gives feedback.
    public private(set) var problemPulse = 0

    /// The banner the viewer shows (nil: nothing wrong).
    public var problem: ViewerProblem? {
        guard let app else { return nil }
        if app.sessionOriginalsMissing, let s = app.currentSession {
            return .sessionMissing(folder: s.name, volume: app.sessionVolumeGone ? app.sessionVolume?.lastPathComponent : nil)
        }
        if let id = app.focusedID, app.isOriginalMissing(id) {
            if app.isVolumeGone(id), let root = app.volumeRoot(of: id) { return .driveDisconnected(volume: root.lastPathComponent) }
            if let other = app.otherVolume(of: id) {
                return .missingOnOtherVolume(name: app.photo(id)?.filename ?? "This photo", volume: other.lastPathComponent)
            }
            return .missingOriginal(name: app.photo(id)?.filename ?? "This photo", canRemove: app.canRemoveMissing(id))
        }
        if let f = renderFailure { return renderFailureIsUnsupported ? .cannotDevelop(f) : .renderFailed(f) }
        return nil
    }

    /// Retry a failed render (the banner's button).
    public func retryRender() {
        AppLog.render.notice("retry requested")
        renderFailure = nil
        renderError = nil
        gpuRetries = 0
        requestRender()
    }

    /// Increments per displayed develop render.
    public private(set) var renderCount = 0

    /// A develop frame put on screen: which photo, which request (monotonic), at
    /// what quality, during a gesture or not (tests record the sequence).
    public struct Presented: Sendable, Equatable {
        public let photo: PhotoID
        public let serial: Int
        public let quality: RenderQuality
        public let frameID: UInt64
        public let duringGesture: Bool
        public let frame: RenderedFrame
    }
    /// Called for every develop frame put on screen (tests).
    @ObservationIgnored public var onPresent: ((Presented) -> Void)?
    /// The request serial of the frame on screen.
    public private(set) var renderedSerial = 0
    /// Increments whenever anything drawn changes (the Metal view re-plans on change).
    public internal(set) var displayVersion = 0

    // Toggles bound to menus/keys.
    public var clippingWarning = false { didSet { displayVersion += 1 } }
    public var focusPeaking = false { didSet { displayVersion += 1 } }
    public var loupe = false { didSet { displayVersion += 1 } }
    /// The soft proof changed (on/off, table, gamut warning): redraw; turning it on or
    /// off also re-renders in (or out of) the working gamut.
    func proofChanged() {
        displayVersion += 1
        let wide = app?.softProof.enabled ?? false
        if wide != proofWide {
            proofWide = wide
            requestRender()
        }
    }
    /// Frames are rendered in the working gamut (Rec.2020) for the soft proof.
    private(set) var proofWide = false
    /// The proof table the Metal view samples (nil: no proof).
    public var proofTable: ProofTable? { app?.softProof.display == nil ? nil : app?.softProof.table }
    public var showNavigator = true

    /// EDR headroom of the window's screen (1 = SDR), reported by the view. With
    /// headroom > 1 and a renderer that emits float frames, renders are 16F linear P3.
    public var edrHeadroom: Double = 1 {
        didSet { if (oldValue > 1) != (edrHeadroom > 1) { requestRender() } }
    }
    /// The window's screen is wide gamut (Display P3), reported by the view: without EDR
    /// headroom, 8-bit renders are P3 there (sRGB otherwise).
    public var displayWideGamut = false {
        didSet { if oldValue != displayWideGamut { requestRender() } }
    }

    // Canvas tools.
    public var canvasTool: CanvasTool = .pan {
        didSet {
            guard canvasTool != oldValue else { return }
            cancelCanvasDrag()
            noteToolChanged(from: oldValue) // crop/straighten session for Esc
            geometryModeMayHaveChanged()
        }
    }
    /// The crop as it was when the crop/straighten tool was entered (Esc restores it).
    @ObservationIgnored var cropSession: CropToolSession?
    public internal(set) var activeDrag: CanvasDrag?
    /// Photo the active drag started on (commits go there or nowhere).
    @ObservationIgnored var dragPhotoID: PhotoID?
    /// Layer selection, tied to the photo it was made on.
    var layerSelection: (photo: PhotoID, layer: Int)?
    /// Brush diameter relative to the long edge.
    public var brushSize = 0.05
    /// Brush edge, flow and edge-aware auto-mask for new strokes.
    public var brushHardness = 0.5
    public var brushFlow = 1.0
    public var brushAutoMask = false

    /// Pixel under the cursor (separately observed; hover never invalidates the viewer).
    @ObservationIgnored public let cursorReadout = CursorReadout()
    /// The eyedroppers' magnifier (separately observed, like the readout).
    @ObservationIgnored public let pickerLoupe = PickerLoupe()
    @ObservationIgnored let loupeJobs = LoupeJobs()
    /// Last cursor position over the viewer (view points). Not observed.
    @ObservationIgnored public var cursor: CGPoint?
    /// Called when the cursor moves and something drawn depends on it (loupe).
    @ObservationIgnored public var onCursorRedraw: (() -> Void)?
    /// Viewer pane in window coordinates (top-left origin); snapshot compositing only.
    @ObservationIgnored public var windowRect: CGRect = .zero

    @ObservationIgnored var renderInFlight = false
    @ObservationIgnored var renderPending = false
    @ObservationIgnored var renderGeneration = 0
    @ObservationIgnored var renderTask: Task<Void, Never>?
    @ObservationIgnored var tileTask: Task<Void, Never>?
    /// The pan-slack prefetch of the latest tile request (an older one is superseded).
    @ObservationIgnored var slackTask: Task<Void, Never>?
    /// The edit `rendered` was rendered from.
    @ObservationIgnored var renderedEdit: EditState?
    /// "Before" renders per photo, with the edit each was rendered from (its framing).
    @ObservationIgnored var beforeCache: [PhotoID: (edit: EditState, frame: RenderedFrame)] = [:]
    @ObservationIgnored private(set) var fastNavigation = false
    /// Hover preview of a preset: only ever applies to the photo it was made for.
    @ObservationIgnored private(set) var previewEditFor: (photo: PhotoID, edit: EditState)?
    /// Neighbours prefetched either side of the focused photo.
    @ObservationIgnored public var prefetchRadius = 3
    /// Delay before a tile render after zoom/pan settles.
    @ObservationIgnored public var tileDelay: Duration = .milliseconds(60)
    @ObservationIgnored package var masks: MaskOverlayCache
    /// Quiet time after the last edit before the draft render is refined (Preview).
    @ObservationIgnored public var settleDelay: Duration = .milliseconds(150)
    @ObservationIgnored var settleTask: Task<Void, Never>?
    /// Quality of the render in flight / asked for next.
    @ObservationIgnored var requestedQuality: RenderQuality = .preview
    /// Quality of the frame on screen (drafts are refined once edits settle).
    public private(set) var renderedQuality: RenderQuality?
    /// No develop render in flight or queued (the frame on screen is the latest state's
    /// once `renderedQuality` is `.preview`). Not observed; for tests and snapshots.
    public var isRenderIdle: Bool { !renderInFlight && !renderPending }
    /// Final-frame mappings (from prefetch or earlier renders), so the embedded preview
    /// is placed in the develop render's geometry before it lands (no jump on swap).
    @ObservationIgnored var mappingCache: [PhotoID: (key: GeometryKey, mapping: FrameMapping)] = [:]
    @ObservationIgnored var mappingOrder: [PhotoID] = []
    @ObservationIgnored var developPrefetches: [PhotoID: Task<Void, Never>] = [:]
    /// Whether the last geometry decision was developed (to re-render on a switch).
    @ObservationIgnored var lastDeveloped: Bool?

    init(renderer: any Renderer, previews: (any PreviewProvider)? = nil, masks: MaskOverlayCache? = nil) {
        self.renderer = renderer
        self.previews = previews
        capabilities = RendererCapabilities.of(renderer)
        previewCache = previews.map { ViewerPreviewCache(provider: $0) }
        self.masks = masks ?? .shared
    }

    /// Recompute an AI mask from the photo's preview (deterministic: same pixels, same request).
    /// A photo without the target (no sky) gets an empty mask; nil means the mask couldn't
    /// be computed (no preview, Vision failed, the sky model is unavailable).
    func regenerateMask(photo: PhotoID, target: AITarget) async -> VisionMask? {
        guard let app, let p = app.photo(photo), let img = await app.analysisImage(for: p) else { return nil }
        do {
            return try await masker.segment(target, img) ?? VisionMask(width: 1, height: 1, bytes: [0])
        } catch {
            return nil
        }
    }

    /// AI mask bitmaps for a develop render (the pipeline's `MaskSource`): cached ones,
    /// else computed with Vision on the photo's preview and cached. A target whose mask
    /// couldn't be computed is left out (the viewer renders it empty; export refuses the
    /// photo).
    func masksForRender(photo: PhotoID, targets: Set<AITarget>) async -> [AITarget: VisionMask] {
        var out: [AITarget: VisionMask] = [:]
        for t in targets {
            if let m = masks.mask(photo: photo, target: t) {
                out[t] = m
            } else if let m = await regenerateMask(photo: photo, target: t) {
                masks.set(m, photo: photo, target: t)
                out[t] = m
            }
        }
        return out
    }

    /// Vision segmentation used to regenerate AI masks (seam for tests).
    @ObservationIgnored public var masker: AIMasker = .vision
    /// People masks (per person and all people; seam for tests).
    @ObservationIgnored var peopleService: PeopleMaskService = .shared
    /// People of per-person masks reported as no longer found ("photo/person"), once each.
    @ObservationIgnored var reportedLostPeople = Set<String>()

    func stop() {
        renderTask?.cancel()
        tileTask?.cancel()
        previewCache?.cancel(except: [])
    }

    /// Stop everything this session's photos started: the render and its
    /// tile, the settle refine, embedded-preview and develop prefetches (in Rust too),
    /// the preset hover preview and the pinned comparison. A canvas tool is left first,
    /// so what it commits is saved with the rest.
    func sessionClosed() {
        leaveCanvasTool()
        resetSessionCaches()
    }

    /// Leave the crop, brush or other canvas tool, so what it commits is saved with the rest.
    func leaveCanvasTool() {
        canvasTool = .pan
        cancelCanvasDrag()
    }

    /// Bumped by `resetSessionCaches()`: a background result started before it is the old
    /// session's and is dropped.
    @ObservationIgnored var sessionEpoch = 0

    /// Stop and forget everything keyed by a photo id of the session being left (ids repeat
    /// across sessions): renders, previews, AI masks, before frames and mappings.
    func resetSessionCaches() {
        sessionEpoch += 1
        cancelRender()
        tileTask?.cancel()
        slackTask?.cancel()
        settleTask?.cancel()
        previewCache?.removeAll()
        masks.removeAll()
        (renderer as? any PrefetchCancelling)?.cancelPrefetches(keeping: [])
        for t in developPrefetches.values { t.cancel() }
        developPrefetches = [:]
        lastPrefetchIndex = nil
        previewEditFor = nil
        layerSelection = nil
        state.comparePinned = nil
        compareFrame = nil
        beforeCache = [:]
        mappingCache = [:]
        mappingOrder = []
        imageAreaCache = [:]
        editGestureActive = false
        clear()
    }

    // MARK: - What is on screen

    /// Develop render of the focused photo, or nil (never another photo's frame, and
    /// never a frame of the other geometry: final frames only while `developedGeometry`).
    public var frame: RenderedFrame? {
        guard renderedID != nil, renderedID == app?.focusedID, let r = rendered else { return nil }
        return (r.mapping != nil) == developedGeometry ? r : nil
    }

    /// The pipeline renders the final geometry and nothing on the canvas needs the
    /// uncropped frame. The crop, straighten and mask tools, and a selected layer's
    /// overlay, work on the oriented image (turned and straightened by the viewer).
    public var developedGeometry: Bool {
        guard capabilities.rendersGeometry else { return false }
        // A crop or mask drag works on the oriented image; an eyedropper click doesn't
        // (it picks what's on screen: switching geometry under it moved the pick point).
        if selectedLayerID != nil { return false }
        if let d = activeDrag, d.tool != .wbPicker, d.tool != .colorPicker, d.tool != .focusPoint { return false }
        switch activeTool {
        case .pan, .wbPicker, .colorPicker, .rotate, .focusPoint: return true
        case .crop, .straighten, .brush, .eraser, .linearGradient, .radialGradient: return false
        }
    }

    /// The final frame's map to crop space for the focused photo while developed: the
    /// frame's own, else a cached one for the same geometry (the preview is then drawn in
    /// the develop render's geometry). Nil: oriented display (emulated turn/straighten).
    public var mapping: FrameMapping? {
        guard developedGeometry, let app, let id = app.focusedID else { return nil }
        if let m = frame?.mapping { return m }
        guard let c = mappingCache[id], c.key == GeometryKey(app.edit(for: id)) else { return nil }
        return c.mapping
    }

    /// The pinned photo's final-frame mapping (compare mode, developed display).
    var compareMapping: FrameMapping? { developedGeometry ? compareFrame?.mapping : nil }

    /// Tool or layer selection changed: re-render if the geometry mode flipped.
    func geometryModeMayHaveChanged() {
        let now = developedGeometry
        defer { lastDeveloped = now }
        guard let last = lastDeveloped, last != now else { return }
        tile = nil
        displayVersion += 1
        requestRender()
        if !now { refreshFinalMapping() }
    }

    func remember(_ mapping: FrameMapping, for id: PhotoID, edit: EditState) {
        mappingCache[id] = (GeometryKey(edit), mapping)
        mappingOrder.removeAll { $0 == id }
        mappingOrder.append(id)
        while mappingOrder.count > 32 { mappingCache[mappingOrder.removeFirst()] = nil }
    }
    public var frameID: PhotoID? { frame == nil ? nil : renderedID }

    /// The frame to draw for the focused photo: its render, else its embedded preview.
    public var displayFrame: RenderedFrame? {
        if let frame { return frame }
        if let previewFrame, previewID != nil, previewID == app?.focusedID { return previewFrame }
        // While an arrow is held, never blank: the last preview stays up
        // until the one for the photo you're on is decoded.
        if fastNavigation, let previewFrame { return previewFrame }
        return nil
    }

    /// True while only the embedded preview is shown.
    public var showingPreview: Bool { frame == nil && displayFrame != nil }

    /// Pixel size used for geometry: metadata, corrected to the displayed frame's
    /// aspect when metadata disagrees (e.g. missing or unrotated dimensions).
    public nonisolated static func imageSize(meta: CGSize, frame: CGSize?) -> CGSize {
        guard let frame, frame.width > 0, frame.height > 0 else { return meta }
        guard meta.width > 0, meta.height > 0 else { return frame }
        // Unrotated metadata: the frame is the same image turned 90°.
        let fa = frame.width / frame.height, swapped = meta.height / meta.width
        return abs(fa - swapped) / swapped < 0.02 && abs(fa - 1) > 0.02 ? CGSize(width: meta.height, height: meta.width) : meta
    }

    /// Oriented pixel size of the focused photo.
    var orientedImageSize: CGSize {
        guard let id = app?.focusedID else { return .zero }
        return orientedImageSize(of: id)
    }

    /// Oriented pixel size of `id` (the displayed frame corrects it only for the focused photo).
    func orientedImageSize(of id: PhotoID) -> CGSize {
        guard let app, let p = app.photo(id) else { return .zero }
        let shown = id == app.focusedID ? displayFrame : nil
        let f = shown.flatMap { $0.mapping == nil ? CGSize(width: $0.width, height: $0.height) : nil }
        return Self.imageSize(meta: p.pixelSize, frame: f)
    }

    /// How the focused photo is displayed (its quarter turns and straighten). Identity
    /// in developed display: the pipeline's frame already is the display.
    public var transform: ViewTransform {
        guard let app, let id = app.focusedID, mapping == nil else { return ViewTransform() }
        return ViewTransform(crop: app.edit(for: id).crop)
    }

    /// Displayed (turned) pixel size of the focused photo: what the layout fits.
    var mainImageSize: CGSize { mapping?.nativeSize ?? transform.displaySize(oriented: orientedImageSize) }

    /// Size of the space tiles and densities are measured in: the final frame in
    /// developed display, else the oriented image.
    var frameSpaceSize: CGSize { mapping?.nativeSize ?? orientedImageSize }

    var displayAspect: CGFloat {
        let s = mainImageSize
        return s.height > 0 ? s.width / s.height : 1
    }

    func pinnedTransform() -> ViewTransform {
        guard let app, let p = state.comparePinned, compareMapping == nil else { return ViewTransform() }
        return ViewTransform(crop: app.edit(for: p).crop)
    }

    /// Layout for the current state (pane-local geometry per pane).
    public var layout: ViewerLayout {
        guard let app, app.focusedPhoto != nil else { return ViewerLayout(panes: []) }
        let pinned = state.comparePinned.flatMap(app.photo).map {
            compareMapping?.nativeSize ?? pinnedTransform().displaySize(oriented: Self.imageSize(
                meta: $0.pixelSize, frame: compareFrame.map { CGSize(width: $0.width, height: $0.height) }))
        }
        return ViewerLayout.make(state: state, main: mainImageSize, pinned: pinned)
    }

    /// Device pixels per image pixel right now.
    public var effectiveScale: Double {
        guard let pane = layout.main else { return 1 }
        return ViewerGeometry.deviceScale(image: mainImageSize, view: pane.frame.size, backingScale: state.backingScale,
                                          zoom: state.zoom)
    }

    /// Texels per image pixel available where the main pane is visible.
    var displayedDensity: Double {
        let size = frameSpaceSize
        guard size.width > 0 else { return 0 }
        if let tile, tile.photoID == app?.focusedID, let region = visibleOrientedRegion,
           tile.region.contains(region.insetBy(dx: 0.0001, dy: 0.0001)) {
            return tile.scale
        }
        return displayFrame.map { Double(max($0.width, $0.height)) / Double(max(size.width, size.height)) } ?? 0
    }

    /// Visible part of the main image in frame space (oriented, or the final frame in
    /// developed display).
    var visibleOrientedRegion: CGRect? {
        guard let pane = layout.main else { return nil }
        if mapping != nil { return pane.visibleRegion.intersection(CGRect(x: 0, y: 0, width: 1, height: 1)) }
        return transform.orientedBounds(of: pane.visibleRegion, displayAspect: displayAspect)
    }

    /// Zoom label, honest about resolution: "Fit", "100%", or "100% (preview)"
    /// when the pixels on screen are upsampled from a smaller frame.
    public var zoomLabel: String {
        Self.zoomLabel(zoom: state.zoom, effective: effectiveScale, density: displayedDensity,
                       developed: capabilities.rendersGeometry, sensorDensity: sensorDensity)
    }

    /// Sensor pixels per pixel of the developed frame. Below 1 when the pipeline
    /// scaled an auto-fitted frame (lens correction, straighten, keystone) back up to the
    /// sensor size (`FillOriginalSize`), so 100% shows upsampled pixels.
    var sensorDensity: Double? {
        guard let m = mapping, m.nativeSize.width > 0 else { return nil }
        let s = orientedImageSize
        guard s.width > 0, s.height > 0 else { return nil }
        let a = m.affine
        return Double(hypot(a[0] * s.width, a[3] * s.height) / m.nativeSize.width)
    }

    /// HUD note when develop renders run on the CPU (no GPU adapter).
    public var backendNote: String? {
        // Live (a GPU lost at runtime switches lt-ffi to the CPU renderer).
        if renderer is DevelopRenderer { return DevelopRenderer.backend.gpu ? nil : "CPU rendering" }
        return capabilities.cpuFallback ? "CPU rendering" : nil
    }

    public nonisolated static func zoomLabel(zoom: ZoomMode, effective: Double, density: Double,
                                             developed: Bool = false, sensorDensity: Double? = nil) -> String {
        guard zoom != .fit else { return "Fit" }
        let pct = "\(Int((effective * 100).rounded()))%"
        // With the pipeline every zoom is developed from the RAW (the 1:1 region follows),
        // but an auto-fitted frame is upsampled against the sensor: say by how much.
        if developed {
            // Up to 1:1 on screen: more screen pixels than sensor pixels means upsampled.
            if let d = sensorDensity, d > 0, effective <= 1.001, effective / d > 1.005 {
                return "\(pct) (upsampled \(Int((effective / d - 1) * 100 + 0.5))%)"
            }
            return pct
        }
        return min(effective, 1) > density * 1.02 ? "\(pct) (preview)" : pct
    }

    /// Everything the Metal view draws, with loupe at the cursor when enabled.
    public func drawPlan(loupeAt cursor: CGPoint?) -> ViewerDrawPlan {
        let l = layout
        var input = ViewerDrawPlan.Input(layout: l, viewSize: state.viewSize, backingScale: state.backingScale,
                                         main: displayFrame, before: state.beforeAfter ? currentBefore : nil,
                                         compare: state.comparePinned == nil ? nil : compareFrame)
        if let tile, tile.photoID == app?.focusedID, state.zoom != .fit { input.tile = (tile.frame, tile.region) }
        input.clippingWarning = clippingWarning
        input.focusPeaking = focusPeaking
        input.imageSize = orientedImageSize
        input.transform = transform
        input.compareTransform = pinnedTransform()
        input.mapping = mapping
        input.compareMapping = compareMapping
        input.wideGamut = displayWideGamut
        input.edrDisplay = renderFormat.isFloat
        let o = orientedImageSize
        input.orientedAspect = o.height > 0 ? o.width / o.height : 1
        if loupe, let cursor, displayFrame != nil { input.loupe = ViewerDrawPlan.Loupe(center: cursor) }
        input.overlayMasks = selectedAIMaskKeys
        input.overlaySegments = overlaySegments
        input.proof = app?.softProof.display
        return ViewerDrawPlan.make(input)
    }

    /// The focused photo's "before", only while it is framed like the develop frame on
    /// screen (a new crop, lens correction or geometry re-renders it instead of a
    /// misaligned split; compared with the frame shown, not the edit asked for, so a
    /// render in flight doesn't blank the split).
    var currentBefore: RenderedFrame? {
        guard let id = app?.focusedID, renderedID == id, let r = rendered, let e = renderedEdit, let b = beforeCache[id],
              b.edit == Self.beforeEdit(for: e, developed: r.mapping != nil) else { return nil }
        return b.frame
    }

    /// Before/after is on but both halves are the same: the photo has no adjustments
    /// (only its framing, which the before keeps). Shown so the split doesn't look broken.
    public var beforeAfterNote: String? {
        guard state.beforeAfter, let app, let id = app.focusedID, app.photo(id)?.isVideo != true else { return nil }
        let edit = shownEdit(for: id)
        return Self.beforeMatchesAfter(edit, developed: developedGeometry) ? "No edits on this photo: before and after are the same" : nil
    }

    nonisolated static func beforeMatchesAfter(_ edit: EditState, developed: Bool) -> Bool {
        var e = edit
        e.personBoxes = [:] // they only qualify mask shapes
        if e.look?.intensity == 0 { e.look = nil }
        if !developed { e.crop = CropEdit() } // the oriented display frames both halves alike
        return e == beforeEdit(for: e, developed: developed)
    }

    /// Groups the edit changes that this renderer can't show (drives the HUD note).
    public var unrenderedGroups: Set<AdjustGroup> {
        guard let app, let id = app.focusedID else { return [] }
        return Self.unrendered(edit: app.edit(for: id), capabilities: capabilities)
    }

    public nonisolated static func unrendered(edit: EditState, capabilities c: RendererCapabilities) -> Set<AdjustGroup> {
        guard c.rendersEdits else { return AdjustGroup.changed(in: edit) }
        guard let shown = c.renderedGroups else { return [] }
        return AdjustGroup.changed(in: edit).subtracting(shown)
    }

    // MARK: - Notifications from AppModel

    /// Focus moved to another photo (or none): show its preview now, cancel the
    /// stale render and any canvas drag, forget per-photo UI state, start a new
    /// render, and prefetch the neighbours.
    func focusChanged() {
        state.zoom = .fit
        state.pan = .zero
        tile = nil
        tileTask?.cancel()
        slackTask?.cancel()
        cancelCanvasDrag()
        previewEditFor = nil
        layerSelection = nil
        cancelRender()
        settleTask?.cancel()
        rendered = nil
        renderedID = nil
        renderError = nil // a damaged file's error never shows over the next photo
        renderFailure = nil
        requestRender(quality: .preview)
        if !fastNavigation { verifyFocusedOriginal() }
        pickerLoupeChanged()
        prefetchNeighbours()
        if let app, masks.count > 0 { masks.retain(photos: Set(app.visibleIDs)) }
        lastDeveloped = developedGeometry
    }

    /// Edits changed for `ids`; re-render if any is on screen.
    func editsChanged(_ ids: Set<PhotoID>) {
        if let app { for id in ids { masks.prune(photo: id, edit: app.edit(for: id)) } }
        if let f = app?.focusedID, ids.contains(f), app?.isOriginalMissing(f) == true {
            // Saved, but it can't be previewed: say so (the banner pulses) instead of
            // a slider that silently does nothing.
            problemPulse += 1
            return
        }
        if let f = app?.focusedID, ids.contains(f) {
            pickerLoupeChanged() // its pixels and readout follow the edit (a WB click)
            if Self.isDisplayOnlyChange(from: lastRequestedEdit, to: shownEdit(for: f), photo: f, developed: developedGeometry,
                                        capabilities: capabilities) {
                // Live straighten/crop: the viewer turns the oriented frame itself (the Metal
                // transform), so a slider tick is a redraw; the render and the final-frame
                // plan (crop box, image area) follow once the drag settles.
                displayVersion += 1
                scheduleSettle()
                return
            }
            tile = nil
            displayVersion += 1
            // Drag in progress: fast drafts now, the full-quality refine once edits settle.
            requestRender(quality: capabilities.rendersEdits && capabilities.rendersGeometry ? .draft : .preview)
            scheduleSettle()
            return
        }
        if let p = state.comparePinned, ids.contains(p) { requestRender() }
    }

    /// Key-repeat navigation: while on, show embedded previews only (no develop
    /// renders); turning it off renders the photo you landed on.
    public func setFastNavigation(_ on: Bool) {
        guard on != fastNavigation else { return }
        fastNavigation = on
        if on {
            cancelRender()
        } else {
            requestRender()
            verifyFocusedOriginal()
            prefetchNeighbours() // the develop prefetch waited for the key-up
        }
    }

    /// Hover preview of a preset/style for the focused photo: render `edit`
    /// instead of the stored one while non-nil. Cleared on navigation.
    public func setPreviewEdit(_ edit: EditState?) {
        let next = edit.flatMap { e in app?.focusedID.map { (photo: $0, edit: e) } }
        guard next?.photo != previewEditFor?.photo || next?.edit != previewEditFor?.edit else { return }
        previewEditFor = next
        tile = nil
        // Hovering styles is a fast sequence of edits: drafts, refined when it settles.
        requestRender(quality: capabilities.rendersEdits && capabilities.rendersGeometry ? .draft : .preview)
        scheduleSettle()
    }

    /// The edit the last develop render was requested with (for `isDisplayOnlyChange`).
    @ObservationIgnored var lastRequestedEdit: (photo: PhotoID, edit: EditState)?

    /// A change the viewer shows by itself, without a develop render: only the crop
    /// (rect, straighten, turns) changed while the canvas shows the oriented frame of a
    /// develop renderer, which renders no crop in that geometry (lt-ffi `for_geometry`).
    nonisolated static func isDisplayOnlyChange(from last: (photo: PhotoID, edit: EditState)?, to edit: EditState,
                                                photo: PhotoID, developed: Bool, capabilities: RendererCapabilities) -> Bool {
        guard !developed, capabilities.rendersGeometry, let last, last.photo == photo, last.edit != edit else { return false }
        var e = edit
        e.crop = last.edit.crop
        return e == last.edit
    }

    /// The edit shown for `id`: the hover preview if it was made for `id`, else the stored edit.
    func shownEdit(for id: PhotoID) -> EditState {
        if let p = previewEditFor, p.photo == id { return p.edit }
        return app?.edit(for: id) ?? EditState()
    }

    public var previewEdit: EditState? {
        guard let p = previewEditFor, p.photo == app?.focusedID else { return nil }
        return p.edit
    }

    // MARK: - Rendering

    /// Frame format for renders: 16F linear P3 on an EDR screen when the renderer can;
    /// otherwise 8-bit matching the display (Display P3 on wide-gamut screens when the
    /// renderer develops, sRGB elsewhere).
    public var renderFormat: FramePixelFormat {
        Self.renderFormat(proof: proofWide, edrHeadroom: edrHeadroom, wideGamut: displayWideGamut,
                          capabilities: capabilities)
    }

    nonisolated static func renderFormat(proof: Bool, edrHeadroom: Double, wideGamut: Bool,
                                         capabilities: RendererCapabilities) -> FramePixelFormat {
        // Soft proofing needs the colours outside P3 (they may be inside the proof profile).
        if proof && capabilities.floatFrames { return .rgba16fLinearRec2020 }
        if edrHeadroom > 1 && capabilities.floatFrames { return .rgba16fLinearP3 }
        return wideGamut && capabilities.rendersGeometry ? .rgba8DisplayP3 : .rgba8Srgb
    }

    /// A slider drag (or another coalesced edit gesture) is under way: the viewer shows
    /// drafts only, each newer than the last, and refines once the gesture ends (drag
    /// flicker: a mid-drag refine alternated Draft and Preview frames).
    public private(set) var editGestureActive = false
    /// Refine anyway when a gesture's end never arrives (a lost mouse-up).
    @ObservationIgnored public var gestureSettleFallback: Duration = .seconds(1)

    func beginEditGesture() {
        guard !editGestureActive else { return }
        editGestureActive = true
        // A refine requested before the drag must not land in the middle of it.
        if renderInFlight, requestedQuality != .draft { cancelRender() }
    }

    func endEditGesture() {
        guard editGestureActive else { return }
        editGestureActive = false
        scheduleSettle() // the one refine, after the idle delay
    }

    /// Refine the draft to Preview once edits stop for `settleDelay` (during a gesture,
    /// only after `gestureSettleFallback`: the gesture's end normally triggers it).
    func scheduleSettle() {
        settleTask?.cancel()
        // Only develop renderers draft; the refine is theirs (other renderers render once).
        guard capabilities.rendersGeometry else { return }
        let delay = editGestureActive ? gestureSettleFallback : settleDelay
        settleTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled, let self else { return }
            self.editGestureActive = false
            self.requestRender(quality: .preview)
            if !self.developedGeometry { self.refreshFinalMapping() }
        }
    }

    /// While the canvas shows the oriented image (crop, straighten, mask tools), plan the
    /// final frame for the current edit so the crop box shows what the pipeline renders
    /// (lens/keystone auto-fit included). Background; redraws when it lands.
    func refreshFinalMapping() {
        guard let dev = renderer as? any GeometryPrefetching, let app, let photo = app.focusedPhoto,
              !photo.isVideo else { return }
        let edit = app.edit(for: photo.id)
        let request = RenderRequest(maxPixelSize: renderPixelSize, format: renderFormat, priority: .background,
                                    geometry: .final)
        // The crop tool also shows the image area: the full frame as the pipeline
        // auto-fits it (straighten, keystone and lens corrections leave no empty corners).
        let full = Self.fullFrame(edit)
        if activeTool == .crop, imageAreaCache[photo.id]?.key != GeometryKey(full) {
            let epoch = sessionEpoch
            Task { [weak self] in
                guard let m = await dev.prefetch(photo, edit: full, request: request), let self,
                      epoch == self.sessionEpoch else { return }
                if self.imageAreaCache.count >= 16 { self.imageAreaCache = [:] }
                self.imageAreaCache[photo.id] = (GeometryKey(full), m)
            }
        }
        if let c = mappingCache[photo.id], c.key == GeometryKey(edit) { return }
        let epoch = sessionEpoch
        Task { [weak self] in
            guard let m = await dev.prefetch(photo, edit: edit, request: request), let self,
                  epoch == self.sessionEpoch else { return }
            self.remember(m, for: photo.id, edit: edit)
            self.displayVersion += 1
        }
    }

    /// `edit` with a full-frame crop (its angle, turns, lens and keystone kept).
    static func fullFrame(_ edit: EditState) -> EditState {
        var e = edit
        e.crop.rect = CGRect(x: 0, y: 0, width: 1, height: 1)
        return e
    }

    /// Planned full-frame mappings (the crop tool's image area), per photo.
    var imageAreaCache: [PhotoID: (key: GeometryKey, mapping: FrameMapping)] = [:]

    /// The crop tool's image area in display space: the auto-fitted full frame, the
    /// part of the oriented image the pipeline fills (lens correction's curved edges and
    /// straighten's corners lie outside it). Nil outside the crop tool or until planned.
    public var cropImageArea: CGRect? {
        guard activeTool == .crop, !developedGeometry, let app, let id = app.focusedID else { return nil }
        let full = Self.fullFrame(app.edit(for: id))
        guard let c = imageAreaCache[id], c.key == GeometryKey(full) else { return nil }
        return ViewTransform(crop: full.crop).renderedDisplayRect(full.crop, displayAspect: displayAspect, mapping: c.mapping)
    }

    /// The final frame's mapping for the focused photo's current edit, if planned.
    var currentFinalMapping: FrameMapping? { app?.focusedID.flatMap(finalMapping(of:)) }

    /// The final frame's mapping for `id`'s current edit, if planned.
    func finalMapping(of id: PhotoID) -> FrameMapping? {
        guard let app, let c = mappingCache[id], c.key == GeometryKey(app.edit(for: id)) else { return nil }
        return c.mapping
    }

    func cancelRender() {
        renderTask?.cancel()
        renderTask = nil
        renderGeneration += 1
        renderInFlight = false
        renderPending = false
    }

    func clear() {
        rendered = nil
        renderedID = nil
        previewFrame = nil
        previewID = nil
        tile = nil
        histogram = nil
        displayVersion += 1
    }

    public func requestRender() { requestRender(quality: .preview) }

    /// Render the focused photo at `quality` (drafts while an edit is being dragged).
    /// A request while a render is in flight is coalesced: the latest state renders next.
    public func requestRender(quality: RenderQuality) {
        guard let app, let photo = app.focusedPhoto else {
            cancelRender()
            clear()
            return
        }
        showPreview(for: photo)
        // No develop for a photo whose original is gone (the banner says so).
        if app.isOriginalMissing(photo.id) {
            cancelRender()
            return
        }
        if fastNavigation { return }
        requestedQuality = quality
        if renderInFlight {
            renderPending = true
            return
        }
        renderInFlight = true
        renderPending = false
        renderGeneration += 1
        let gen = renderGeneration
        let edit = shownEdit(for: photo.id)
        lastRequestedEdit = (photo.id, edit)
        let pinned = state.comparePinned.flatMap(app.photo)
        let pinnedEdit = pinned.map { app.edit(for: $0.id) }
        let developed = developedGeometry
        let request = RenderRequest(maxPixelSize: renderPixelSize, format: renderFormat, quality: quality,
                                    priority: .interactive, geometry: developed ? .final : .oriented, histogram: true)
        let beforeEdit = Self.beforeEdit(for: edit, developed: developed)
        let cachedBefore = beforeCache[photo.id].flatMap { c in
            c.edit == beforeEdit && max(c.frame.width, c.frame.height) >= request.maxPixelSize - 1
                && (c.frame.mapping != nil) == developed ? c.frame : nil
        }
        // Drafts of a drag skip the before (one render per tick); the settle renders it.
        let wantBefore = state.beforeAfter && cachedBefore == nil && !(editGestureActive && quality == .draft)
        let renderer = renderer
        renderTask = Task { [weak self] in
            do {
                async let main = renderer.render(photo, edit: edit, request: request)
                async let before: RenderedFrame? = wantBefore ? renderer.render(photo, edit: beforeEdit, request: request) : nil
                async let cmp = Self.renderIfPresent(renderer, pinned, edit: pinnedEdit, request: request)
                let (f, b, c) = try await (main, before, cmp)
                try Task.checkCancellation()
                self?.finishRender(gen, photo.id, frame: f, before: (b ?? cachedBefore).map { (beforeEdit, $0) }, compare: c,
                                  quality: quality, edit: edit)
            } catch {
                self?.failRender(gen, error)
            }
        }
    }

    /// The "before" of before/after: the defaults, framed like the edit when the
    /// pipeline renders the final geometry (same crop, straighten and lens framing).
    nonisolated static func beforeEdit(for edit: EditState, developed: Bool) -> EditState {
        var b = EditState()
        if developed {
            b.crop = edit.crop
            b.lens = edit.lens
        }
        return b
    }

    nonisolated private static func renderIfPresent(_ r: any Renderer, _ p: Photo?, edit: EditState?,
                                                    request: RenderRequest) async throws -> RenderedFrame? {
        guard let p else { return nil }
        return try await r.render(p, edit: edit ?? EditState(), request: request)
    }

    /// Long edge of the fit render: the image's fitted size in device pixels, so
    /// the fit frame is drawn about 1:1 and needs no mips (zero-copy textures).
    var renderPixelSize: Int {
        let view = state.viewSize
        guard view.width > 0, view.height > 0 else { return 512 }
        let image = mainImageSize
        let px: CGFloat
        if image.width > 0, image.height > 0 {
            let fit = ViewerGeometry.imageRect(image: image, view: view, backingScale: state.backingScale, zoom: .fit, pan: .zero)
            px = max(fit.width, fit.height) * state.backingScale
        } else {
            px = max(view.width, view.height) * state.backingScale
        }
        return min(max(Int(px.rounded(.up)), 256), 6144)
    }

    func finishRender(_ gen: Int, _ id: PhotoID, frame: RenderedFrame, before: (edit: EditState, frame: RenderedFrame)?,
                              compare: RenderedFrame?, quality: RenderQuality = .preview, edit: EditState? = nil) {
        guard gen == renderGeneration else { return } // superseded (cancelled) render
        renderInFlight = false
        guard id == app?.focusedID else {
            requestRender(quality: requestedQuality)
            return
        }
        // Never a frame older than the one on screen (renders are serialised, so this is
        // a guard, not a reorder).
        guard id != renderedID || gen >= renderedSerial else { return }
        renderCount += 1
        renderedSerial = gen
        onPresent?(Presented(photo: id, serial: gen, quality: quality, frameID: frame.id, duringGesture: editGestureActive,
                                    frame: frame))
        rendered = frame
        renderedID = id
        renderedEdit = edit
        renderedQuality = quality
        if let m = frame.mapping, let edit { remember(m, for: id, edit: edit) }
        if let before {
            beforeCache[id] = before
            if beforeCache.count > 4, let k = beforeCache.keys.first(where: { $0 != id }) { beforeCache[k] = nil }
        }
        beforeFrame = before?.frame
        compareFrame = compare
        updateHistogram(from: frame)
        if frame.mapping != nil, loupeKind != nil, pickerLoupe.kind == nil { pickerLoupeChanged() } // its mapping is here
        renderError = nil
        renderFailure = nil
        gpuRetries = 0
        displayVersion += 1
        if renderPending {
            requestRender(quality: requestedQuality)
        } else if quality != .draft {
            scheduleTile()
        }
    }

    /// Histogram of what's on screen. Develop frames carry theirs (binned in the
    /// pipeline's output pass: no pass over the pixels here); previews and other
    /// renderers' frames are binned off the critical path (float frames from an SDR copy).
    func updateHistogram(from f: RenderedFrame) {
        if let h = f.histogram {
            histogram = h
            return
        }
        Task { [weak self] in
            let h = await Task.detached(priority: .userInitiated) { HistogramEngine.shared.histogram(for: f) }.value
            guard let self, self.displayFrame == f else { return }
            self.histogram = h
        }
    }

    func failRender(_ gen: Int, _ error: Error) {
        guard gen == renderGeneration else { return }
        renderInFlight = false
        if !(error is CancellationError) {
            renderError = error.localizedDescription
            noteFailure(error)
        }
        if OpenFailures.isDamaged(error), let id = app?.focusedID { app?.markDamaged(id) }
        if renderPending {
            requestRender(quality: requestedQuality)
        } else if Self.isTransientGPUError(error), gpuRetries < 2 {
            // A lost GPU device is rebuilt on the next render (or lt-ffi falls back
            // to the CPU): retry instead of leaving the error on screen.
            gpuRetries += 1
            AppLog.render.notice("retrying the develop after a GPU error (\(self.gpuRetries) of 2)")
            let quality = requestedQuality
            Task { [weak self] in
                try? await Task.sleep(for: .milliseconds(250))
                guard let self, self.renderGeneration == gen else { return }
                self.requestRender(quality: quality)
            }
        }
    }

    /// Log the failure, and classify it: a missing original (the photo is marked, the
    /// banner offers Locate/Remove) or a render failure (the banner offers Retry).
    func noteFailure(_ error: Error) {
        guard let app, let id = app.focusedID else { return }
        let name = app.photo(id)?.filename ?? "?"
        let message = error.localizedDescription
        AppLog.render.error("develop of \(name, privacy: .private(mask: .hash)) failed: \(message, privacy: .private(mask: .hash))")
        if OpenFailures.isMissingFile(error) {
            // "No such file" may be about some other file (a cache, a model): a check of
            // the original itself says whether it's missing; otherwise it's a render
            // failure. (Demo photos have no files to check.)
            if app.mode == .live { verifyFocusedOriginal(failure: message) } else { originalMissing(id) }
            return
        }
        // A transient GPU loss retries on its own (failRender); anything else is shown.
        if !Self.isTransientGPUError(error) || gpuRetries >= 2 {
            renderFailure = message
            renderFailureIsUnsupported = OpenFailures.isUnsupportedFormat(error)
        }
        // The file may be gone even when the error doesn't say so (a drive unmounted).
        verifyFocusedOriginal()
    }

    @ObservationIgnored var originalCheck: Task<Void, Never>?
    /// Checks an original and its volume off the main thread (nil: no answer in time).
    /// Tests replace it.
    @ObservationIgnored public var originalProbe: @Sendable (URL) async -> OriginalProbe? = OriginalProbe.live

    /// The focused photo's original may have gone (deleted, its drive unmounted) or come
    /// back since it was listed: check off the main thread, bounded and rationed (a hung
    /// network drive never stalls anything, nor piles up threads), and say so instead of
    /// developing a file that isn't there. `failure`: the render error that prompted the
    /// check, shown when the original turns out to be there (or can't be checked).
    func verifyFocusedOriginal(failure: String? = nil) {
        // Live files only (demo photos have placeholder paths).
        guard let app, app.mode == .live, let id = app.focusedID, let p = app.photo(id), !p.isVideo,
              let url = p.url else {
            if let failure { renderFailure = failure }
            return
        }
        let storeMissing = p.isMissing
        let probe = originalProbe
        originalCheck?.cancel()
        originalCheck = Task { [weak self] in
            let found = await probe(url)
            guard !Task.isCancelled, let self, let app = self.app, app.focusedID == id else { return }
            guard let found else {
                if let failure { self.renderFailure = failure }
                return
            }
            if let root = VolumeMount.root(of: url), let mounted = found.volumeMounted {
                if mounted { app.originalVolumeMounted(root) } else { app.noteVolume(root, mounted: false) }
            }
            if found.exists {
                if storeMissing {
                    app.storeMissingFound(id) // back: the store re-checks it
                } else if app.noteOriginal(id, missing: false) {
                    self.requestRender() // back (a drive plugged in again): develop it
                } else if let failure {
                    self.renderFailure = failure
                }
            } else if !storeMissing {
                self.originalMissing(id)
            }
        }
    }

    /// A drive came back: develop the focused photo again.
    func originalBack() {
        guard app?.focusedID != nil else { return }
        requestRender()
        verifyFocusedOriginal()
    }

    /// `id`'s original isn't there: mark it, and when it's the focused photo drop its
    /// develop (a render from cached pixels would contradict the banner) and keep the
    /// embedded preview with the banner over it.
    func originalMissing(_ id: PhotoID) {
        guard let app else { return }
        app.noteOriginal(id, missing: true)
        guard app.focusedID == id else { return }
        cancelRender()
        settleTask?.cancel()
        renderFailure = nil
        renderError = nil
        rendered = nil
        renderedID = nil
        tile = nil
        displayVersion += 1
    }

    /// Retries after GPU errors since the last good frame.
    @ObservationIgnored var gpuRetries = 0

    nonisolated static func isTransientGPUError(_ error: Error) -> Bool {
        guard let e = error as? RenderError, e.kind == .internalError else { return false }
        return e.message.localizedCaseInsensitiveContains("gpu") || e.message.localizedCaseInsensitiveContains("device")
    }

}
