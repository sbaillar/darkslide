import CoreGraphics
import Foundation
import Observation

/// A crop ratio of the user's own (Capture One's user aspect ratios), beside the fixed
/// chips. Applying one cuts the largest centred box of that ratio once; the crop stays
/// free afterwards (lt-core's aspect lock keeps only the built-in ratios).
public struct CustomCropRatio: Codable, Equatable, Identifiable, Sendable {
    public var id: UUID
    public var name: String
    /// Width : height as Capture One stored it (its orientation is kept).
    public var width: Double
    public var height: Double

    public init(id: UUID = UUID(), name: String, width: Double, height: Double) {
        self.id = id
        self.name = name
        self.width = width
        self.height = height
    }

    /// Pixel width / height; nil when unusable.
    public var ratio: CGFloat? {
        guard width.isFinite, height.isFinite, width > 0, height > 0 else { return nil }
        return CGFloat(width / height)
    }
}

/// Saved custom crop ratios (`UserDefaults` suite, JSON). Unreadable data reads as none.
@MainActor @Observable
public final class CropRatioStore {
    public static let key = "customCropRatios"
    public static let shared = CropRatioStore(defaults: AppModel.appDefaults)

    @ObservationIgnored private let defaults: UserDefaults
    public private(set) var ratios: [CustomCropRatio]

    public init(defaults: UserDefaults) {
        self.defaults = defaults
        ratios = defaults.data(forKey: Self.key).flatMap { try? JSONDecoder().decode([CustomCropRatio].self, from: $0) } ?? []
    }

    public func ratio(_ id: UUID) -> CustomCropRatio? { ratios.first { $0.id == id } }

    /// Add, or replace the ratio with the same id.
    public func put(_ r: CustomCropRatio) {
        var list = ratios
        if let i = list.firstIndex(where: { $0.id == r.id }) { list[i] = r } else { list.append(r) }
        set(list)
    }

    public func remove(_ id: UUID) { set(ratios.filter { $0.id != id }) }

    private func set(_ list: [CustomCropRatio]) {
        guard list != ratios else { return }
        ratios = list
        defaults.set(try? JSONEncoder().encode(list), forKey: Self.key)
    }
}

extension AppModel {
    /// Crop the focused photo to a custom ratio: the largest centred box of that ratio
    /// inside the current crop, in one undo step. The aspect stays free.
    public func applyCropRatio(_ r: CustomCropRatio) {
        guard let id = focusedID, photo(id)?.isVideo != true, let ratio = r.ratio else { return }
        let v = viewer
        updateEdits([id], label: "Crop \(r.name)") { id, e in
            e.crop.aspect = .free
            let box = v.cropBox(for: e, photo: id)
            v.setCropBox(CropGeometry.fit(box.rect, ratio: ratio, size: box.size), in: &e, photo: id)
        }
        endEditGesture()
    }
}
