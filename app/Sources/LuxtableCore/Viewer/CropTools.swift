import CoreGraphics
import Foundation
import ImageIO
import Vision

/// The aspect chips of the crop tool (C2/C3). A preset is a stored `CropAspect` plus an
/// orientation: lt-core keeps only long:short, the crop rect carries the orientation.
public enum CropPreset: String, CaseIterable, Sendable, Identifiable {
    case original = "Original", square = "1:1", r4x5 = "4:5", r5x4 = "5:4", r3x2 = "3:2", r2x3 = "2:3"
    case r16x9 = "16:9", r9x16 = "9:16", free = "Free"
    case r4x3 = "4:3", r3x4 = "3:4", r7x5 = "7:5", r5x7 = "5:7", r8x5 = "8:5", r5x8 = "5:8"

    public var id: String { rawValue }

    public var aspect: CropAspect {
        switch self {
        case .original: .original
        case .square: .r1x1
        case .r4x5, .r5x4: .r4x5
        case .r3x2, .r2x3: .r3x2
        case .r16x9, .r9x16: .r16x9
        case .r4x3, .r3x4: .r4x3
        case .r7x5, .r5x7: .r7x5
        case .r8x5, .r5x8: .r8x5
        case .free: .free
        }
    }

    /// Fixed orientation of the ratio in displayed pixels; nil for 1:1, Original, Free.
    public var portrait: Bool? {
        switch self {
        case .r4x5, .r2x3, .r9x16, .r3x4, .r5x7, .r5x8: true
        case .r5x4, .r3x2, .r16x9, .r4x3, .r7x5, .r8x5: false
        case .square, .original, .free: nil
        }
    }

    /// X: the same ratio turned 90° (Original and Free stay).
    public var swapped: CropPreset {
        switch self {
        case .r4x5: .r5x4
        case .r5x4: .r4x5
        case .r3x2: .r2x3
        case .r2x3: .r3x2
        case .r16x9: .r9x16
        case .r9x16: .r16x9
        case .r4x3: .r3x4
        case .r3x4: .r4x3
        case .r7x5: .r5x7
        case .r5x7: .r7x5
        case .r8x5: .r5x8
        case .r5x8: .r8x5
        case .square, .original, .free: self
        }
    }

    /// The preset a stored aspect and a displayed crop box correspond to.
    public static func matching(_ aspect: CropAspect, portrait: Bool) -> CropPreset {
        switch aspect {
        case .original: .original
        case .free: .free
        case .r1x1: .square
        case .r4x5: portrait ? .r4x5 : .r5x4
        case .r3x2: portrait ? .r2x3 : .r3x2
        case .r16x9: portrait ? .r9x16 : .r16x9
        case .r4x3: portrait ? .r3x4 : .r4x3
        case .r7x5: portrait ? .r5x7 : .r7x5
        case .r8x5: portrait ? .r5x8 : .r8x5
        }
    }
}

/// The crop tool's ratio, chosen from the Crop button's menu or the Crop panel and kept
/// (Settings) until the user picks another: every crop drawn or resized holds it, on every
/// photo. Orientation isn't part of it: a new box follows the drag, X turns the box.
public enum CropRatio: String, CaseIterable, Sendable, Identifiable {
    case original, free, square, r4x3, r3x2, r16x9, r5x4, r7x5, r8x5

    public var id: String { rawValue }

    /// Short name (menu rows, panel chips).
    public var title: String {
        switch self {
        case .original: "Original"
        case .free: "Freeform"
        case .square: "Square"
        case .r4x3: "4:3"
        case .r3x2: "3:2"
        case .r16x9: "16:9"
        case .r5x4: "4 × 5"
        case .r7x5: "5 × 7"
        case .r8x5: "5 × 8"
        }
    }

    /// What it is, where the title alone doesn't say.
    public var detail: String? {
        switch self {
        case .original: "The photo's own shape"
        case .free: "Any shape"
        case .r5x4: "Also 8 × 10"
        case .r7x5, .r8x5: "Print size"
        default: nil
        }
    }

    public var aspect: CropAspect {
        switch self {
        case .original: .original
        case .free: .free
        case .square: .r1x1
        case .r4x3: .r4x3
        case .r3x2: .r3x2
        case .r16x9: .r16x9
        case .r5x4: .r4x5
        case .r7x5: .r7x5
        case .r8x5: .r8x5
        }
    }

    /// The preset of this ratio in an orientation.
    public func preset(portrait: Bool) -> CropPreset {
        CropPreset.matching(aspect, portrait: portrait)
    }

    /// The ratio a preset belongs to.
    public init(_ preset: CropPreset) {
        self = Self.allCases.first { $0.aspect == preset.aspect } ?? .original
    }
}

/// Pure crop geometry in normalized display coordinates (`size` = displayed pixels).
public enum CropGeometry {
    static let unit = CGRect(x: 0, y: 0, width: 1, height: 1)

    /// Is the box portrait in pixels?
    public static func isPortrait(_ r: CGRect, size: CGSize) -> Bool {
        r.height * size.height > r.width * size.width
    }

    /// The largest rect of `ratio` (pixel width / height) centred in `r`.
    public static func fit(_ r: CGRect, ratio: CGFloat, size: CGSize) -> CGRect {
        guard ratio > 0, size.width > 0, size.height > 0, r.width > 0, r.height > 0 else { return r }
        var pw = r.width * size.width, ph = r.height * size.height
        if pw / ph > ratio { pw = ph * ratio } else { ph = pw / ratio }
        let w = pw / size.width, h = ph / size.height
        return CGRect(x: r.midX - w / 2, y: r.midY - h / 2, width: w, height: h)
    }

    /// X: the box turned 90° about its centre (same pixel size), shrunk to stay inside
    /// the image with the new ratio, then kept inside by moving.
    public static func swapOrientation(_ r: CGRect, size: CGSize) -> CGRect {
        guard size.width > 0, size.height > 0, r.width > 0, r.height > 0 else { return r }
        let pw = r.height * size.height, ph = r.width * size.width // swapped pixel extents
        var w = pw / size.width, h = ph / size.height
        let s = min(1, 1 / w, 1 / h)
        w *= s
        h *= s
        return CanvasInteraction.clampRect(CGRect(x: r.midX - w / 2, y: r.midY - h / 2, width: w, height: h))
    }

    /// Pixel width / height of a preset's lock for a box `r` (orientation from the preset,
    /// else from the box; Original = the whole image's ratio). Nil: no lock.
    public static func ratio(_ preset: CropPreset, box r: CGRect, size: CGSize) -> CGFloat? {
        switch preset {
        case .free: return nil
        case .original: return size.height > 0 ? size.width / size.height : nil
        default:
            guard let a = preset.aspect.ratio else { return nil }
            return CanvasInteraction.lockRatio(a, portrait: preset.portrait, w: r.width * size.width, h: r.height * size.height)
        }
    }
}

// MARK: - Viewer: the crop / straighten tool session

/// What entering the crop or straighten tool saved, so Esc can put it back.
struct CropToolSession: Equatable {
    var photo: PhotoID
    var crop: CropEdit
}

extension ViewerModel {
    /// Display-space crop geometry of `edit` independent of the display mode: the box
    /// (normalized to the straightened, turned image) and that image's pixel size.
    /// `photo`: whose image (default the focused one).
    func cropBox(for edit: EditState, photo: PhotoID? = nil) -> (rect: CGRect, size: CGSize) {
        let t = ViewTransform(crop: edit.crop)
        let size = t.displaySize(oriented: photo.map(orientedImageSize(of:)) ?? orientedImageSize)
        let aspect = size.height > 0 ? size.width / size.height : 1
        let mapping = if let photo { finalMapping(of: photo) } else { currentFinalMapping }
        return (t.renderedDisplayRect(edit.crop, displayAspect: aspect, mapping: mapping), size)
    }

    /// Store a display-space box as `edit`'s canonical crop rect.
    func setCropBox(_ r: CGRect, in e: inout EditState, photo: PhotoID? = nil) {
        let t = ViewTransform(crop: e.crop)
        let size = t.displaySize(oriented: photo.map(orientedImageSize(of:)) ?? orientedImageSize)
        let aspect = size.height > 0 ? size.width / size.height : 1
        e.crop.rect = t.canonicalRect(display: r, displayAspect: aspect)
    }

    /// The preset the focused photo's crop corresponds to (chip highlight).
    public var cropPreset: CropPreset {
        guard let app, let id = app.focusedID else { return .original }
        let e = app.edit(for: id)
        let box = cropBox(for: e)
        return CropPreset.matching(e.crop.aspect, portrait: CropGeometry.isPortrait(box.rect, size: box.size))
    }

    /// Aspect lock a crop drag uses: the crop tool's ratio (`settings.cropRatio`; by default
    /// Original, the photo's own shape) in the box's current orientation, or the drag's own
    /// when `drawing` a new box. Freeform drags freely.
    func cropLock(for e: EditState, drawing: Bool = false) -> (aspect: (long: Int, short: Int)?, portrait: Bool?) {
        let box = cropBox(for: e)
        let ratio: (long: Int, short: Int)?
        switch app?.settings.cropRatio ?? .original {
        case .original:
            let w = Int(box.size.width.rounded()), h = Int(box.size.height.rounded())
            ratio = w > 0 && h > 0 ? (max(w, h), min(w, h)) : nil
        case let r:
            ratio = r.aspect.ratio
        }
        guard let ratio else { return (nil, nil) }
        return (ratio, drawing || ratio.long == ratio.short ? nil : CropGeometry.isPortrait(box.rect, size: box.size))
    }

    /// Enter or leave a composition tool (C / R / I, toolbar). Entering crop or straighten
    /// remembers the crop so Esc can restore it; leaving by toggling keeps the edit.
    public func toggleTool(_ tool: CanvasTool) {
        if canvasTool == tool {
            commitTool()
        } else {
            canvasTool = tool
        }
    }

    /// Enter/Return: keep the crop and go back to panning (one closed undo step).
    public func commitTool() {
        cancelCanvasDrag()
        cropSession = nil
        canvasTool = .pan
        app?.endEditGesture()
    }

    /// Esc: put the crop back as it was when the tool was entered, and leave the tool.
    public func cancelTool() {
        cancelCanvasDrag()
        if let s = cropSession, let app, app.focusedID == s.photo, app.edit(for: s.photo).crop != s.crop {
            app.updateEdits([s.photo], label: "Cancel Crop") { _, e in e.crop = s.crop }
            app.endEditGesture()
        }
        cropSession = nil
        canvasTool = .pan
    }

    /// Remember the crop when a crop/straighten tool starts on the focused photo.
    func noteToolChanged(from old: CanvasTool) {
        let composing: Set<CanvasTool> = [.crop, .straighten]
        if composing.contains(canvasTool), let app, let id = app.focusedID {
            if cropSession?.photo != id || !composing.contains(old) {
                cropSession = CropToolSession(photo: id, crop: app.edit(for: id).crop)
            }
        } else if !composing.contains(canvasTool) {
            cropSession = nil
        }
    }

    /// Keys the crop and straighten tools own: ⏎ commits, Esc cancels (a drag in progress
    /// first), X swaps the crop orientation. Returns false for everything else.
    public func handleToolKey(_ key: KeyInput) -> Bool {
        guard canvasTool == .crop || canvasTool == .straighten, !key.command, !key.option, !key.control else { return false }
        switch key.special {
        case .returnKey:
            commitTool()
            return true
        case .escape:
            if activeDrag != nil { cancelCanvasDrag() } else { cancelTool() }
            return true
        default:
            break
        }
        if canvasTool == .crop, key.special == nil, key.characters.lowercased() == "x" {
            app?.swapCropOrientation()
            return true
        }
        return false
    }
}

// MARK: - App: aspect presets, orientation swap, auto straighten

extension AppModel {
    /// Pick an aspect preset (chip, NL "crop 4:5"): the largest centred rect of that ratio
    /// inside the current crop, in the preset's orientation, in one undo step (C3).
    /// Original goes back to the whole frame; Free only unlocks. `id`: the photo to crop
    /// (default the focused one; ⌘K passes the photo the command was typed for, H3).
    public func setCropAspect(_ preset: CropPreset, for id: PhotoID? = nil) {
        guard let id = id ?? focusedID, photo(id)?.isVideo != true else { return }
        settings.cropRatio = CropRatio(preset)
        updateEdits([id], label: "Crop \(preset.rawValue)") { id, e in crop(&e, to: preset, photo: id) }
        endEditGesture()
    }

    /// Pick the crop tool's ratio (the Crop button's menu, the Crop panel). It stays for every
    /// later crop until another is picked, and the focused photo's crop becomes the largest
    /// centred box of that ratio inside its current crop, in the box's orientation (one undo
    /// step). Original is the photo's own shape; Freeform only unlocks.
    public func chooseCropRatio(_ r: CropRatio) {
        settings.cropRatio = r
        guard let id = focusedID, photo(id)?.isVideo != true else { return }
        let box = viewer.cropBox(for: edit(for: id))
        let preset = r.preset(portrait: CropGeometry.isPortrait(box.rect, size: box.size))
        let v = viewer
        updateEdits([id], label: "Crop \(r.title)") { id, e in
            guard r == .original else { return crop(&e, to: preset, photo: id) }
            e.crop.aspect = .original
            let b = v.cropBox(for: e, photo: id)
            guard let ratio = CropGeometry.ratio(.original, box: b.rect, size: b.size),
                  abs(b.rect.width * b.size.width - ratio * b.rect.height * b.size.height) > 0.5 else { return }
            v.setCropBox(CropGeometry.fit(b.rect, ratio: ratio, size: b.size), in: &e, photo: id)
        }
        endEditGesture()
    }

    /// `setCropAspect`'s change to one edit (⌘K folds it into its own undo step, N7).
    func crop(_ e: inout EditState, to preset: CropPreset, photo id: PhotoID) {
        e.crop.aspect = preset.aspect
        switch preset {
        case .free: return
        case .original: e.crop.rect = CGRect(x: 0, y: 0, width: 1, height: 1)
        default:
            let box = viewer.cropBox(for: e, photo: id)
            guard let ratio = CropGeometry.ratio(preset, box: box.rect, size: box.size) else { return }
            viewer.setCropBox(CropGeometry.fit(box.rect, ratio: ratio, size: box.size), in: &e, photo: id)
        }
    }

    /// X: turn the crop box 90° (4:5 ↔ 5:4, free boxes too), keeping it inside the image.
    public func swapCropOrientation() {
        guard let id = focusedID, photo(id)?.isVideo != true else { return }
        let v = viewer
        updateEdits([id], label: "Swap Crop Orientation") { _, e in
            let box = v.cropBox(for: e)
            var r = CropGeometry.swapOrientation(box.rect, size: box.size)
            // A locked ratio stays exact after the shrink-to-fit.
            if let a = e.crop.aspect.ratio {
                let ratio = CanvasInteraction.lockRatio(a, portrait: !CropGeometry.isPortrait(box.rect, size: box.size),
                                                        w: 1, h: 1)
                r = CropGeometry.fit(r, ratio: e.crop.aspect == .r1x1 ? 1 : ratio, size: box.size)
            }
            v.setCropBox(r, in: &e)
        }
        endEditGesture()
    }

    /// Auto Straighten (button, NL "straighten"): level the horizon Vision finds in the
    /// oriented image. Low-confidence or implausible horizons change nothing and say so.
    /// Returns the angle applied (lt-core degrees), or nil. `id`: the photo to level
    /// (default the focused one; ⌘K passes the photo the command was typed for, H3).
    @discardableResult
    public func autoStraighten(for id: PhotoID? = nil) async -> Double? {
        guard let p = (id ?? focusedID).flatMap(photo), !p.isVideo else { return nil }
        let found: Double?
        do {
            found = try await horizonAngle(for: p)
        } catch {
            show(error)
            return nil
        }
        guard let angle = found else {
            show(Toast("No clear horizon found; nothing was straightened", symbol: "level"))
            return nil
        }
        updateEdits([p.id], label: "Auto Straighten") { _, e in e.crop.angle = angle }
        endEditGesture()
        show(Toast(String(format: "Straightened %@%.1f°", angle >= 0 ? "+" : "−", abs(angle)), symbol: "level"))
        return angle
    }

    /// The angle that levels `p`'s horizon, or nil when Vision found no confident,
    /// plausible one (more than 20° is a slope, not a tilt).
    func horizonAngle(for p: Photo) async throws -> Double? {
        guard let h = try await horizonDetector.horizon(of: p, images: images),
              h.confidence >= HorizonReading.minConfidence, abs(h.degrees) <= 20 else { return nil }
        // A horizon rising to the right by h is levelled by a clockwise turn: angle -h.
        return min(max(-h.degrees, -45), 45)
    }
}

// MARK: - Horizon detection

/// A measured horizon: tilt in degrees (positive = rises to the right) and Vision's confidence.
public struct HorizonReading: Equatable, Sendable {
    public var degrees: Double
    public var confidence: Double
    /// Below this, Auto Straighten leaves the photo alone.
    public static let minConfidence = 0.5

    public init(degrees: Double, confidence: Double) {
        self.degrees = degrees
        self.confidence = confidence
    }
}

public protocol HorizonDetecting: Sendable {
    /// The horizon of the photo's oriented image, or nil when there is none.
    func horizon(of photo: Photo, images: any ImageProvider) async throws -> HorizonReading?
}

/// Vision's horizon detector on a 1024 px oriented thumbnail.
public struct VisionHorizonDetector: HorizonDetecting {
    public init() {}

    public func horizon(of photo: Photo, images: any ImageProvider) async throws -> HorizonReading? {
        let image = try await images.thumbnail(for: photo, maxPixelSize: 1024)
        return try await Task.detached(priority: .userInitiated) { try Self.detect(image) }.value
    }

    /// Vision's `angle` is the correcting rotation (the negative of the tilt).
    static func detect(_ image: CGImage) throws -> HorizonReading? {
        let req = VNDetectHorizonRequest()
        try VNImageRequestHandler(cgImage: image).perform([req])
        return req.results?.first.map { HorizonReading(degrees: -Double($0.angle) * 180 / .pi, confidence: Double($0.confidence)) }
    }
}
