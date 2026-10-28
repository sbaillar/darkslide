import LuxtableAI
import LuxtableCore
import MetalKit
import SwiftUI

/// Center viewer: Metal surface + canvas overlay + HUD + gestures.
struct ViewerContainer: View {
    @Bindable var model: AppModel
    @Environment(\.displayScale) private var displayScale
    @Environment(\.colorScheme) private var colorScheme
    @State private var dragLast: CGPoint?
    @State private var draggingSplit = false

    private var viewer: ViewerModel { model.viewer }

    var body: some View {
        GeometryReader { geo in
            ZStack {
                if let photo = model.focusedPhoto {
                    surface
                        .accessibilityElement()
                        .accessibilityLabel(PhotoAccessibility.viewerLabel(photo))
                        .accessibilityValue("zoom \(viewer.zoomLabel)")
                        .accessibilityAddTraits(.isImage)
                        .accessibilityAdjustableAction { dir in
                            if dir == .increment { viewer.zoomIn() } else { viewer.zoomOut() }
                        }
                        .overlay { CanvasOverlay(viewer: viewer) }
                        .overlay { ViewerHUD(model: model) }
                        .overlay(alignment: .bottomTrailing) { NavigatorOverlay(viewer: viewer).padding(DS.Space.m) }
                        .overlay { PickerLoupeView(loupe: viewer.pickerLoupe) } // eyedropper magnifier
                        .contentShape(.rect)
                        .gesture(dragGesture)
                        .simultaneousGesture(SpatialTapGesture(count: 2).onEnded { v in
                            if viewer.activeTool == .pan {
                                viewer.hover(at: v.location)
                                viewer.toggleZoomAtCursor()
                            }
                        })
                        .simultaneousGesture(SpatialTapGesture(count: 1).onEnded { v in tap(at: v.location) })
                        .onContinuousHover { phase in
                            // Snapshots stage the cursor themselves; the real pointer would move it.
                            guard !model.snapshotMode else { return }
                            if case .active(let p) = phase { viewer.hover(at: p) } else { viewer.hover(at: nil) }
                        }
                        // The pointer stays visible with the loupe (a crosshair): hiding it
                        // (pointerVisibility) starves the hover of mouse moves on macOS 27.
                        .pointerStyle(pointer)
                } else {
                    ViewerEmptyState(model: model)
                }
            }
            .overlay(alignment: .top) { ProblemBanner(model: model) }
            .onAppear {
                viewer.setViewerSize(geo.size, backingScale: displayScale)
                viewer.windowRect = geo.frame(in: .global)
                viewer.edrHeadroom = Self.edrHeadroom()
                viewer.displayWideGamut = Self.wideGamut()
            }
            .onChange(of: geo.size) { _, s in
                viewer.setViewerSize(s, backingScale: displayScale)
                viewer.windowRect = geo.frame(in: .global)
            }
            .onReceive(NotificationCenter.default.publisher(for: PlatformDisplay.screenChanged)) { _ in
                viewer.edrHeadroom = Self.edrHeadroom()
                viewer.displayWideGamut = Self.wideGamut()
            }
        }
        .background(DS.viewerBackground)
        .clipped()
    }

    /// The screen showing the app covers Display P3 (8-bit renders are P3 there).
    static func wideGamut() -> Bool { PlatformDisplay.wideGamut() }

    /// EDR headroom of the screen showing the app (1 = SDR only).
    static func edrHeadroom() -> Double { PlatformDisplay.edrHeadroom() }

    @ViewBuilder private var surface: some View {
        if model.snapshotMode {
            // cacheDisplay can't read a CAMetalLayer: snapshots draw the same developed
            // frame with SwiftUI, *under* the overlays (crop box, masks, handles).
            SnapshotFrameLayer(viewer: viewer)
        } else {
            MetalImageView(viewer: viewer, version: viewer.displayVersion, clearColor: clearColor)
        }
    }

    /// One pointer per tool (SwiftUI manages the cursor stack: no unbalanced push/pop).
    private var pointer: PointerStyle? {
        switch viewer.activeTool {
        case .pan: viewer.state.zoom == .fit ? nil : .grabIdle
        case .crop, .linearGradient, .radialGradient, .straighten: .rectSelection
        // With the loupe, the system crosshair: its hot spot is exactly the pick point (the
        // eyedropper symbol's tip isn't at its corner, so it pointed a few points off).
        case .colorPicker, .wbPicker:
            viewer.pickerLoupe.shown ? .rectSelection : .image(Image(systemName: "eyedropper"), hotSpot: .bottomLeading)
        case .brush, .eraser: .image(Image(systemName: "circle"), hotSpot: .center)
        case .rotate: nil
        case .focusPoint: .image(Image(systemName: "scope"), hotSpot: .center)
        }
    }

    /// The surround is specified in sRGB; Metal clears in linear.
    private var clearColor: MTLClearColor {
        let v = colorScheme == .dark ? 0.105 : 0.80
        let lin = ColorMath.linear(v)
        return MTLClearColor(red: lin, green: lin, blue: lin, alpha: 1)
    }

    private func tap(at p: CGPoint) {
        ControlFocus.shared.resignAll() // a click on the photo hands the arrows back to culling
        switch viewer.activeTool {
        case .wbPicker, .colorPicker, .focusPoint:
            viewer.beginCanvasDrag(at: p)
            viewer.endCanvasDrag()
        default: break
        }
    }

    private var dragGesture: some Gesture {
        DragGesture(minimumDistance: 2)
            .onChanged { v in
                if dragLast == nil { ControlFocus.shared.resignAll() }
                if viewer.activeTool != .pan {
                    if dragLast == nil { viewer.beginCanvasDrag(at: v.startLocation) }
                    dragLast = v.location
                    viewer.updateCanvasDrag(to: v.location)
                    return
                }
                if dragLast == nil {
                    dragLast = v.startLocation
                    draggingSplit = viewer.grabsSplit(atX: v.startLocation.x)
                }
                if draggingSplit {
                    viewer.dragSplit(toX: v.location.x)
                } else {
                    let prev = dragLast ?? v.startLocation
                    viewer.hover(at: v.location)
                    viewer.pan(by: CGSize(width: v.location.x - prev.x, height: v.location.y - prev.y))
                    dragLast = v.location
                }
            }
            .onEnded { _ in
                if viewer.activeTool != .pan { viewer.endCanvasDrag() }
                dragLast = nil
                draggingSplit = false
            }
    }
}

/// Labels, split handle, zoom badge, readouts, and hints over the image.
struct ViewerHUD: View {
    @Bindable var model: AppModel

    var body: some View {
        let viewer = model.viewer
        let layout = viewer.layout
        ZStack(alignment: .topLeading) {
            ForEach(Array(layout.panes.enumerated()), id: \.offset) { _, pane in
                if let x = pane.splitX {
                    SplitHandle()
                        .position(x: x, y: pane.frame.midY)
                    hudLabel("Before").position(x: max(x - 44, pane.image.minX + 40), y: pane.frame.minY + 24)
                    hudLabel("After").position(x: min(x + 40, pane.image.maxX - 40), y: pane.frame.minY + 24)
                }
                if layout.panes.count > 1 {
                    hudLabel(pane.role == .pinned ? "Pinned" : "Current",
                             symbol: pane.role == .pinned ? "pin.fill" : nil)
                        .position(x: pane.frame.midX, y: pane.frame.minY + 24)
                }
            }
            VStack(alignment: .leading, spacing: DS.Space.s) {
                if viewer.activeTool == .wbPicker {
                    hudLabel("Click a neutral gray or white area · scroll to magnify · Esc when done", symbol: "eyedropper")
                } else if viewer.activeTool == .focusPoint {
                    hudLabel("Click what should be in focus · Esc when done", symbol: "scope")
                } else if viewer.activeTool != .pan {
                    hudLabel(viewer.activeTool.viewerLabel, symbol: viewer.activeTool.viewerSymbol)
                }
                if let note = viewer.beforeAfterNote {
                    hudLabel(note, symbol: "square.split.2x1")
                }
                // Snapshots show the app as the full develop renders it, not the demo
                // renderer's limits (docs/screenshots).
                if !model.snapshotMode,
                   let note = ViewerNotes.editsNotShown(viewer.unrenderedGroups, capabilities: viewer.capabilities) {
                    hudLabel(note.text, symbol: "info.circle").help(note.help)
                }
                if let note = viewer.backendNote {
                    hudLabel(note, symbol: "cpu").help("No GPU is available: the develop pipeline runs on the CPU (slower).")
                }
                if let proof = model.softProof.statusText {
                    hudLabel(proof, symbol: "printer")
                        .help("Soft proof (⇧S): the photo as it will look in this profile")
                }
                if viewer.showingPreview {
                    hudLabel("Embedded preview", symbol: "bolt.fill")
                        .help("Showing the camera's JPEG while the develop render finishes")
                }
                // Render failures and missing originals: the ProblemBanner (with actions).
            }
            .padding(DS.Space.m)
            if let note = AICullApplication.rejectNote(model.focusedID.flatMap(model.photo)) {
                hudLabel("AI reject: \(note)", symbol: "sparkles")
                    .padding(DS.Space.m)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
            } else if let cull = model.focusedID.flatMap({ model.cullSuggestions[$0] }), !cull.reasons.isEmpty {
                hudLabel(cull.reasons.joined(separator: " · "), symbol: "sparkles")
                    .padding(DS.Space.m)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
            }
            VStack(alignment: .trailing, spacing: DS.Space.s) {
                if viewer.state.zoom != .fit {
                    hudLabel(viewer.zoomLabel, symbol: "plus.magnifyingglass")
                        .help(viewer.capabilities.rendersGeometry
                              ? "Zoom. Pixels are developed from the RAW; at 100% and above they are the sensor's own."
                              : "Zoom. “(preview)” means the pixels shown are upscaled while the 1:1 render loads.")
                }
                if viewer.activeTool == .colorPicker || viewer.loupe {
                    ReadoutChip(readout: viewer.cursorReadout)
                }
            }
            .padding(DS.Space.m)
            // The AI Optimize panel floats at the window's top right: the HUD moves to its left
            // (the top left has the tool labels).
            .padding(.trailing, model.optimizePanel == nil ? 0 : OptimizePanelView.width + DS.Space.m)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
        }
        .allowsHitTesting(false)
    }

    private func hudLabel(_ text: String, symbol: String? = nil) -> some View {
        HUDLabel(text: text, symbol: symbol)
    }
}

struct HUDLabel: View {
    let text: String
    var symbol: String?

    var body: some View {
        HStack(spacing: DS.Space.xs) {
            if let symbol { Image(systemName: symbol) }
            Text(text)
        }
        .font(.system(size: 11, weight: .medium))
        .foregroundStyle(.white)
        .padding(.horizontal, 10)
        .padding(.vertical, 5)
        // Dark scrim under the glass: readable over bright and dark photos alike.
        .background(.black.opacity(0.55), in: .capsule)
        .glassEffect(.regular, in: .capsule)
    }
}

/// RGB/Lab under the cursor; only this view observes the readout.
struct ReadoutChip: View {
    let readout: CursorReadout

    var body: some View {
        VStack(alignment: .trailing, spacing: 2) {
            if let rgb = readout.rgbText { Text(rgb) }
            if let lab = readout.labText { Text(lab).foregroundStyle(DS.textSecondary) }
            if let p = readout.pinned {
                Text(String(format: "Picked %3.0f %3.0f %3.0f", p.x * 255, p.y * 255, p.z * 255))
                    .foregroundStyle(DS.accent)
            }
        }
        .font(.system(size: 10, weight: .medium).monospacedDigit())
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .glassEffect(.regular, in: .rect(cornerRadius: DS.Radius.m))
    }
}

struct SplitHandle: View {
    var body: some View {
        Circle()
            .fill(.white)
            .frame(width: 22, height: 22)
            .shadow(color: .black.opacity(0.4), radius: 3, y: 1)
            .overlay(Image(systemName: "arrow.left.and.right").font(.system(size: 9, weight: .bold)).foregroundStyle(.black))
    }
}

/// Shown when there is nothing to view: explains what to do next.
struct ViewerEmptyState: View {
    @Bindable var model: AppModel

    var body: some View {
        let state = model.emptyState
        VStack(spacing: DS.Space.m) {
            if state == .loading {
                ProgressView().controlSize(.large)
                Text(state.title).foregroundStyle(DS.textSecondary)
            } else {
                EmptyStateView(symbol: state.symbol, title: state.title, message: state.message,
                               actions: model.emptyStateActions.map { a in (a.title, { run(a) }) })
                if state == .recents {
                    // Nothing was reopened (none usable, or the last one is gone): the recents.
                    RecentSessionsList(model: model).frame(maxWidth: 420)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func run(_ action: EmptyState.Action) {
        guard !model.performEmptyStateAction(action) else { return }
        OpenFolderPanel.run(model: model)
    }
}

/// Icon, title, one-line explanation, actions (the first one prominent).
struct EmptyStateView: View {
    let symbol: String
    let title: String
    let message: String
    let actions: [(String, () -> Void)]

    var body: some View {
        VStack(spacing: DS.Space.m) {
            Image(systemName: symbol)
                .font(.system(size: 44, weight: .light))
                .foregroundStyle(DS.textTertiary)
            Text(title).font(DS.Font.title)
            Text(message)
                .foregroundStyle(DS.textSecondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
            if let first = actions.first {
                HStack(spacing: DS.Space.s) {
                    Button(first.0, action: first.1)
                        .buttonStyle(.borderedProminent)
                        .tint(DS.accent)
                    ForEach(actions.dropFirst().map(\.0), id: \.self) { title in
                        Button(title) { actions.first { $0.0 == title }?.1() }
                            .buttonStyle(.bordered)
                    }
                }
                .controlSize(.large)
                .padding(.top, DS.Space.s)
            }
        }
        .padding(DS.Space.xl)
    }
}

/// The developed frame (and its 1:1 tile) drawn with SwiftUI, for `--snapshot` captures.
package struct SnapshotFrameLayer: View {
    let viewer: ViewerModel

    package var body: some View {
        ZStack(alignment: .topLeading) {
            Color.clear
            if let pane = viewer.layout.main, let img = viewer.displayFrame?.makeCGImage() {
                let t = viewer.transform
                Self.oriented(Image(decorative: img, scale: 1).resizable().interpolation(.high), in: pane.image, t)
                if viewer.state.zoom != .fit, let tile = viewer.tile, tile.frame.mapping != nil,
                   let t = tile.frame.makeCGImage() {
                    let r = tile.region
                    Image(decorative: t, scale: 1)
                        .resizable()
                        .interpolation(.none)
                        .frame(width: r.width * pane.image.width, height: r.height * pane.image.height)
                        .offset(x: pane.image.minX + r.minX * pane.image.width, y: pane.image.minY + r.minY * pane.image.height)
                }
                // The selected layer's AI masks, tinted like the Metal pass does (it isn't
                // captured here, so without this a sky mask never showed in snapshots).
                ForEach(viewer.selectedAIMaskKeys, id: \.self) { key in
                    if let tint = viewer.masks.mask(photo: key.photo, target: key.target).flatMap(Self.tint) {
                        Self.oriented(Image(decorative: tint, scale: 1).resizable().interpolation(.medium), in: pane.image, t)
                    }
                }
            }
        }
        .clipped()
        .allowsHitTesting(false)
        .id(viewer.displayVersion)
    }

    /// An oriented-space image placed like the Metal shader places it: turned and
    /// straightened about the displayed rect's center (the crop tool shows it rotated).
    private static func oriented(_ image: some View, in rect: CGRect, _ t: ViewTransform) -> some View {
        let s = t.orientedSize(display: rect.size)
        return image
            .frame(width: s.width, height: s.height)
            .rotationEffect(.degrees(t.displayRotationDegrees))
            .frame(width: rect.width, height: rect.height)
            .offset(x: rect.minX, y: rect.minY)
    }

    /// A mask as the shader's tint: red at 35 % where selected (premultiplied RGBA).
    static func tint(_ m: VisionMask) -> CGImage? {
        guard m.width > 0, m.height > 0, m.bytes.count >= m.width * m.height else { return nil }
        var px = [UInt8](repeating: 0, count: m.width * m.height * 4)
        for i in 0..<(m.width * m.height) {
            let a = Double(m.bytes[i]) / 255 * 0.35
            px[i * 4] = UInt8(255 * a)
            px[i * 4 + 1] = UInt8(26 * a)
            px[i * 4 + 2] = UInt8(26 * a)
            px[i * 4 + 3] = UInt8(255 * a)
        }
        guard let provider = CGDataProvider(data: Data(px) as CFData) else { return nil }
        return CGImage(width: m.width, height: m.height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: m.width * 4,
                       space: CGColorSpace(name: CGColorSpace.sRGB)!,
                       bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                       provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
    }
}

/// The welcome state's recent sessions: an unavailable one is greyed out with why.
struct RecentSessionsList: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(model.recentEntries) { e in
                Button { LocatePanel.openRecent(e.session, model: model) } label: {
                    HStack {
                        Image(systemName: e.session.unavailable == nil ? "folder" : "questionmark.folder")
                        Text(e.name).lineLimit(1)
                        Spacer()
                        Text(e.reason ?? AppModel.sessionSubtitle(e.session)).font(DS.Font.caption).lineLimit(1)
                            .truncationMode(.middle)
                            .foregroundStyle(DS.textSecondary)
                    }
                    .padding(.horizontal, DS.Space.s).padding(.vertical, 4)
                    .contentShape(.rect)
                }
                .buttonStyle(.plain)
                .disabled(!model.canOpenRecent)
                .opacity(e.session.unavailable == nil ? 1 : 0.5)
                .help(LocatePanel.help(e.session))
                .accessibilityLabel("\(e.name), \(e.reason ?? PanelChrome.photoCount(e.session.photoCount))")
            }
        }
    }
}
