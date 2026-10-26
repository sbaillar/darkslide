import CoreGraphics
import Foundation

extension AppModel {
    /// The lens blur's depth map of photo `id` (see `DepthMapService.depth`), computed from
    /// its analysis image.
    func depthForRender(_ id: PhotoID, wait: Bool, service: DepthMapService = .shared) async -> DepthResult? {
        guard let photo = photo(id), photo.url != nil, !photo.isVideo else { return nil }
        // The photo on screen and anything waiting go ahead of the grid's maps.
        return await service.depth(for: photo, wait: wait, urgent: wait || id == focusedID) { [weak self] in
            await self?.analysisImage(for: photo)
        }
    }

    /// A depth map landed for `id`: its thumbnail and, on screen, the photo render their
    /// lens blur with it.
    func depthReady(_ id: PhotoID) {
        guard edit(for: id).effects.lensBlur.needsDepth else { return }
        depthVersions[id, default: 0] += 1
        guard id == focusedID else { return }
        viewer.editsChanged([id])
    }

    /// Wire depth maps into every render path (live mode).
    func wireDepth(service: DepthMapService = .shared) {
        service.onDepthReady = { [weak self] id in self?.depthReady(id) }
        // A grid cell that scrolled away stops its photo's background map.
        thumbnails.leftScreen = { [weak self] id in
            guard id != self?.focusedID else { return }
            service.leftScreen(id)
        }
        DepthWiring.provider = { [weak self] id, wait in
            guard let self else { return nil }
            return await self.depthForRender(id, wait: wait, service: service)
        }
    }

    /// Lens Blur panel: download the depth model; the photo on screen gets real depth.
    public func downloadDepthModel(service: DepthMapService = .shared) {
        service.download { [weak self] in
            guard let self, let id = self.focusedID else { return }
            self.depthReady(id)
        }
    }
}
