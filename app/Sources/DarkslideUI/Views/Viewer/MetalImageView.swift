import DarkslideCore
import MetalKit
import SwiftUI

/// Metal-backed image surface. Draws on demand (no display link while idle).
/// All decisions (what to draw where, filtering, EDR) come from
/// `ViewerModel.drawPlan`; textures come from `TextureUploader` (off-main, cached).
/// The platform representable (NSView or UIView) and `ViewerMTKView` are in
/// `Views/Platform/MetalImageView+Platform.swift`.
@MainActor
struct MetalImageView {
    let viewer: ViewerModel
    /// Changes whenever anything drawn changes (observed by SwiftUI).
    let version: Int
    let clearColor: MTLClearColor

    func makeCoordinator() -> Coordinator { Coordinator(viewer: viewer) }

    /// The surface, configured for `c` (both platforms).
    func makeView(_ c: Coordinator) -> ViewerMTKView {
        let view = ViewerMTKView(frame: .zero, device: c.uploader?.renderer.device)
        view.colorPixelFormat = .bgra8Unorm_srgb
        view.colorspace = CGColorSpace(name: CGColorSpace.sRGB)
        view.enableSetNeedsDisplay = true
        view.isPaused = true
        view.framebufferOnly = true
        view.delegate = c
        view.setLayerOpaque()
        view.onZoom = { [weak viewer] factor, point in viewer?.zoom(by: factor, at: point) }
        view.onScrollPan = { [weak viewer] delta in viewer?.pan(by: delta) }
        view.onLoupeScroll = { [weak viewer] dy in
            guard let loupe = viewer?.pickerLoupe, loupe.shown else { return false }
            loupe.scroll(by: dy)
            return true
        }
        c.view = view
        viewer.onCursorRedraw = { [weak view] in view?.requestRedraw() }
        return view
    }

    func update(_ view: ViewerMTKView) {
        view.clearColor = clearColor
        view.requestRedraw()
    }

    static func dismantle(_ coordinator: Coordinator) {
        coordinator.viewer.onCursorRedraw = nil
    }

    @MainActor
    final class Coordinator: NSObject, MTKViewDelegate {
        let viewer: ViewerModel
        let uploader: TextureUploader?
        let maskTextures: MaskTextureCache?
        /// The proof table's 3D texture, kept while its table is current.
        private var proofTexture: (id: UInt64, texture: MTLTexture)?
        weak var view: MTKView?
        /// Each pane's last presented texture (never a blank pane while an upload runs).
        private var held = HeldTextures()
        private var edr = false
        private var wide = false

        init(viewer: ViewerModel) {
            self.viewer = viewer
            uploader = (try? MetalViewerRenderer(pixelFormat: .bgra8Unorm_srgb)).map { TextureUploader(renderer: $0) }
            maskTextures = uploader.map { MaskTextureCache(device: $0.renderer.device) }
        }

        private func proofTexture(for table: ProofTable?, device: MTLDevice) -> MTLTexture? {
            guard let table else { proofTexture = nil; return nil }
            if let t = proofTexture, t.id == table.id { return t.texture }
            let tex = MetalViewerRenderer.makeProofTexture(device: device, size: table.size, rgba: table.rgba)
            proofTexture = tex.map { (table.id, $0) }
            return tex
        }

        nonisolated func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

        nonisolated func draw(in view: MTKView) {
            MainActor.assumeIsolated { render(view) }
        }

        /// Switch the surface between SDR sRGB, SDR Display P3 (wide-gamut screens) and
        /// EDR (rgba16Float, extended linear P3).
        private func configure(_ view: MTKView, edr: Bool, wide: Bool) {
            guard edr != self.edr || wide != self.wide else { return }
            self.edr = edr
            self.wide = wide
            view.colorPixelFormat = edr ? .rgba16Float : .bgra8Unorm_srgb
            let space = edr ? CGColorSpace.extendedLinearDisplayP3 : wide ? CGColorSpace.displayP3 : CGColorSpace.sRGB
            view.colorspace = CGColorSpace(name: space)
            (view.layer as? CAMetalLayer)?.wantsExtendedDynamicRangeContent = edr
        }

        private func render(_ view: MTKView) {
            guard let uploader else { return }
            let plan = viewer.drawPlan(loupeAt: viewer.loupe ? viewer.cursor : nil)
            configure(view, edr: plan.edr, wide: plan.wide)
            guard let pass = view.currentRenderPassDescriptor, let drawable = view.currentDrawable,
                  let cb = uploader.renderer.makeCommandBuffer() else { return }
            let redraw: @Sendable () -> Void = { [weak view] in
                DispatchQueue.main.async { view?.requestRedraw() }
            }
            var panes: [MetalViewerRenderer.Pane] = []
            let proofLUT = proofTexture(for: viewer.proofTable, device: uploader.renderer.device)
            var keep = plan.frameIDs
            held.trim(to: plan.quads.count)
            for (slot, q) in plan.quads.enumerated() {
                // Not uploaded yet: the pane keeps what it showed (the upload's completion
                // redraws); never an empty pass in between.
                let ready = uploader.texture(for: q.frame, mipmapped: q.mipmapped, onReady: redraw)
                let (shown, heldID) = held.resolve(slot: slot, frame: q.frame, region: q.region, ready: ready)
                guard let tex = shown else { continue }
                if let heldID { keep.insert(heldID) }
                var p = MetalViewerRenderer.Pane(after: tex, rect: q.rect, split: q.split, clip: q.clip)
                if let b = q.before { p.before = uploader.texture(for: b, mipmapped: q.mipmapped, onReady: redraw) }
                if q.before != nil, p.before == nil { p.split = -2 }
                p.span = q.span
                p.region = q.region
                p.quarterTurns = q.quarterTurns
                p.angle = q.angle
                p.aspect = q.aspect
                p.peakStep = q.peakStep
                p.nearest = q.nearest
                p.circle = q.circle
                p.clippingWarning = q.clippingWarning
                p.focusPeaking = q.focusPeaking
                p.colorMatrix = q.colorMatrix
                p.segments = q.segments
                p.displaySpace = q.displaySpace
                if let a = q.cropAffine, a.count == 6 { p.cropAffine = (SIMD3(a[0], a[1], a[2]), SIMD3(a[3], a[4], a[5])) }
                p.cropAspect = q.cropAspect
                if let proof = q.proof, let proofLUT, proof.tableID == viewer.proofTable?.id {
                    p.proofLUT = proofLUT
                    p.proofIn = q.proofIn
                    p.proofOut = q.proofOut
                    p.gamutWarning = proof.gamutWarning
                    p.gamutColor = q.gamutColor
                    p.neutralTolerance = q.neutralTolerance
                }
                p.masks = q.masks.compactMap { k in
                    viewer.masks.mask(photo: k.photo, target: k.target).flatMap { maskTextures?.texture(for: k, mask: $0) }
                }
                panes.append(p)
            }
            uploader.renderer.encode(panes, pass: pass, commandBuffer: cb, viewPixelWidth: Int(view.drawableSize.width),
                                     targetFormat: view.colorPixelFormat)
            // N3: textures leaving the screen are recycled only after the GPU is done with them.
            uploader.retain(only: keep, after: cb)
            cb.present(drawable)
            cb.commit()
        }
    }
}
