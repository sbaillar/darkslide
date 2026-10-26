import CoreGraphics
import LuxtableAI
import Foundation

extension ViewerModel {
    // MARK: - Previews

    func showPreview(for photo: Photo) {
        guard let cache = previewCache else { return }
        if previewID != photo.id {
            previewID = photo.id
            // A held arrow keeps the last frame until this one is ready.
            if !fastNavigation || cache.entry(photo.id) != nil { previewFrame = nil }
        }
        cache.load(photo) { [weak self] entry in
            guard let self, let entry, self.app?.focusedID == photo.id, self.previewID == photo.id else { return }
            if self.previewFrame != entry.frame {
                self.previewFrame = entry.frame
                self.displayVersion += 1
                if self.frame == nil { self.updateHistogram(from: entry.frame) }
            }
        }
    }

    /// Prefetch embedded previews ±`prefetchRadius` around the focused photo;
    /// cancel prefetches that fell out of the window.
    func prefetchNeighbours() {
        guard let cache = previewCache, let app, let f = app.focusedID, let i = app.visibleIndex(of: f) else { return }
        let ids = app.visibleIDs
        // Which way the user is going, and how far ahead: a held arrow
        // (30 Hz) prefetches `fastPrefetchAhead` previews that way and only one back,
        // and never starts develop prefetches (a RAW decode each) that compete with it.
        let forward = lastPrefetchIndex.map { i >= $0 } ?? true
        lastPrefetchIndex = i
        let ahead = fastNavigation ? fastPrefetchAhead : prefetchRadius
        let back = fastNavigation ? 1 : prefetchRadius
        let (before, after) = forward ? (back, ahead) : (ahead, back)
        let lo = max(0, i - before), hi = min(ids.count - 1, i + after)
        guard lo <= hi else { return }
        cache.cancel(except: Set(ids[lo...hi]))
        // Nearest first, in the direction of travel first.
        for k in 1...max(ahead, back, 1) {
            for j in (forward ? [i + k, i - k] : [i - k, i + k]) where j >= lo && j <= hi {
                if let p = app.photo(ids[j]) { cache.load(p, priority: fastNavigation ? .userInitiated : .utility) { _ in } }
            }
        }
        if !fastNavigation { prefetchDevelop(around: i, in: ids) }
    }

    /// Warm the develop pipeline (decode + GPU BASE, background priority) for the
    /// nearest neighbours and remember their final-frame mapping, so navigating there
    /// shows the preview in the develop geometry and the render lands without a jump.
    func prefetchDevelop(around i: Int, in ids: [PhotoID]) {
        guard let dev = renderer as? any GeometryPrefetching, let app else { return }
        let wanted = [i + 1, i - 1, i + 2].filter { $0 >= 0 && $0 < ids.count }.map { ids[$0] }
        // The old neighbourhood's develop prefetches (decode + BASE) stop in Rust too.
        if let c = renderer as? any PrefetchCancelling {
            c.cancelPrefetches(keeping: ([ids[i]] + wanted).compactMap { app.photo($0) })
        }
        for (id, t) in developPrefetches where !wanted.contains(id) {
            t.cancel()
            developPrefetches[id] = nil
        }
        let size = renderPixelSize, format = renderFormat
        for id in wanted where developPrefetches[id] == nil {
            guard let p = app.photo(id), !p.isVideo else { continue }
            let edit = app.edit(for: id)
            if let c = mappingCache[id], c.key == GeometryKey(edit) { continue }
            let request = RenderRequest(maxPixelSize: size, format: format, priority: .background, geometry: .final)
            developPrefetches[id] = Task { [weak self] in
                let m = await dev.prefetch(p, edit: edit, request: request)
                guard let self else { return }
                self.developPrefetches[id] = nil
                if let m, !Task.isCancelled { self.remember(m, for: id, edit: edit) }
            }
        }
    }

    /// Small image for the navigator, turned like the display: the embedded preview,
    /// else the displayed frame. 8-bit frames are cheap and made here; float (EDR) frames
    /// are scaled and converted on the GPU off the main thread (`NavigatorThumbnail`,
    /// gpu-audit #1), showing the previous image until the new one lands.
    public var navigatorImage: CGImage? {
        _ = navigatorGeneration
        let turns = transform.quarterTurns
        let frame: RenderedFrame?
        let key: String
        if mapping != nil, let f = displayFrame, f.mapping != nil {
            frame = f // developed display: the navigator shows the final frame
            key = "f\(f.id)/\(turns)"
        } else if let id = app?.focusedID, let e = previewCache?.entry(id) {
            key = "p\(id.raw)/\(turns)"
            if let c = navigatorCache, c.key == key { return c.image }
            guard let img = Self.turned(e.image, quarterTurns: turns) else { return nil }
            navigatorCache = (key, img)
            return img
        } else if let f = displayFrame {
            frame = f
            key = "f\(f.id)/\(turns)"
        } else {
            return nil
        }
        if let c = navigatorCache, c.key == key { return c.image }
        guard let f = frame else { return nil }
        if f.format.isEightBit {
            guard let img = f.makeCGImage().flatMap({ Self.turned($0, quarterTurns: turns) }) else { return nil }
            navigatorCache = (key, img)
            return img
        }
        if navigatorPending != key {
            navigatorPending = key
            Task { [weak self] in
                let img = await Task.detached(priority: .userInitiated) {
                    NavigatorThumbnail.make(f).flatMap { ViewerModel.turned($0, quarterTurns: turns) }
                }.value
                guard let self, self.navigatorPending == key else { return }
                self.navigatorPending = nil
                if let img {
                    self.navigatorCache = (key, img)
                    self.navigatorGeneration += 1
                }
            }
        }
        return navigatorCache?.image
    }

    /// `img` rotated clockwise by `quarterTurns`.
    nonisolated static func turned(_ img: CGImage, quarterTurns k: Int) -> CGImage? {
        let k = ((k % 4) + 4) % 4
        guard k != 0 else { return img }
        let (w, h) = k % 2 == 1 ? (img.height, img.width) : (img.width, img.height)
        guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return nil }
        // CG is y-up: rotating the content clockwise on screen is a negative rotation here.
        ctx.translateBy(x: CGFloat(w) / 2, y: CGFloat(h) / 2)
        ctx.rotate(by: -CGFloat(k) * .pi / 2)
        ctx.draw(img, in: CGRect(x: -CGFloat(img.width) / 2, y: -CGFloat(img.height) / 2,
                                 width: CGFloat(img.width), height: CGFloat(img.height)))
        return ctx.makeImage()
    }

    /// Whether the embedded preview of `id` is already decoded (tests, metrics).
    public func hasPreview(_ id: PhotoID) -> Bool { previewCache?.entry(id) != nil }
}
