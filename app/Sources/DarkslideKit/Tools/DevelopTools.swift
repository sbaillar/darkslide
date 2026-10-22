import CoreGraphics
import DarkslideFFI
import Foundation
import ImageIO

/// A white balance as the UI shows it (Kelvin + green/magenta tint).
public struct WhiteBalanceValue: Equatable, Sendable {
    public var kelvin: Double
    public var tint: Double
    public init(kelvin: Double, tint: Double) {
        self.kelvin = kelvin
        self.tint = tint
    }
}

/// What the develop tools know about one original.
public struct PhotoDevelopInfo: Equatable, Sendable {
    /// The camera's white balance through the camera matrix (`multipliers_to_wb(as_shot)`).
    public var asShot: WhiteBalanceValue
    /// EXIF lens model.
    public var lens: String?
    /// The lens-correction profile the pipeline applies, e.g. "Sony FE 24-70mm F2.8 GM".
    public var lensProfile: String?
    /// ISO (noise reduction adapts to it).
    public var iso: Int?
    /// "Apple M4 Pro" or "CPU rendering (…)".
    public var renderer: String

    public init(asShot: WhiteBalanceValue, lens: String?, lensProfile: String?, iso: Int?, renderer: String) {
        self.asShot = asShot
        self.lens = lens
        self.lensProfile = lensProfile
        self.iso = iso
        self.renderer = renderer
    }
}

/// Develop tools on the shared pipeline renderer (live mode: `RustDevelopTools`).
/// Demo mode has none; the app falls back to its preview-based estimates.
/// Every call throws for a photo without a file.
public protocol DevelopTools: Sendable {
    func info(for photo: Photo) async throws -> PhotoDevelopInfo
    /// Auto white balance from the camera data (GPU statistics).
    func autoWhiteBalance(for photo: Photo) async throws -> WhiteBalanceValue
    /// `edit` with exposure, HDR and levels set by auto tone (GPU probe renders).
    func autoTone(for photo: Photo, edit: EditState) async throws -> EditState
    /// The white balance that makes the `sample`×`sample` full-resolution pixels around
    /// `point` (normalized in the final, cropped and turned frame of the develop render of
    /// `edit`) neutral. Deterministic: the loupe shows the value a click then applies.
    func pickWhiteBalance(for photo: Photo, edit: EditState, at point: CGPoint, sample: Int) async throws -> WhiteBalanceValue
    /// Grid thumbnail of an edited photo (pipeline render at background priority,
    /// disk-cached). `cached` tells whether it came from the cache.
    func editedThumbnail(for photo: Photo, edit: EditState) async throws -> (image: CGImage, cached: Bool)
}

extension EditState {
    /// Adopt auto tone's exposure, HDR and levels (the rest stays as it was).
    public func withTone(from other: EditState) -> EditState {
        var e = self
        e.exposure = other.exposure
        e.hdr = other.hdr
        e.levels = other.levels
        return e
    }
}

extension WhiteBalanceEdit {
    /// A custom white balance with these values.
    public static func custom(_ v: WhiteBalanceValue) -> WhiteBalanceEdit {
        var wb = WhiteBalanceEdit()
        wb.asShot = false
        wb.temperature = v.kelvin.rounded()
        wb.tint = (v.tint * 10).rounded() / 10
        return wb
    }
}

/// Readout texts of the develop tools (pure, tested).
public enum WhiteBalanceReadout {
    /// "As shot: 5,450 K, tint +3.2" (the camera's white balance through its matrix).
    public static func asShot(_ v: WhiteBalanceValue) -> String {
        let t = (v.tint * 10).rounded() / 10
        let tint = t == 0 ? "0" : String(format: "%+.1f", t)
        return "As shot: \(Formatters.kelvin(v.kelvin.rounded())), tint \(tint)"
    }
}

public enum LensReadout {
    /// The applied profile ("Sony FE 24-70mm F2.8 GM"), else why none applies.
    public static func profile(info: PhotoDevelopInfo?, exifLens: String?, chosen: String? = nil) -> String {
        if let chosen { return "\(chosen) (chosen)" }
        if let p = info?.lensProfile { return p }
        let lens = exifLens.flatMap { $0.isEmpty ? nil : $0 }
        guard info != nil else { return lens ?? "–" }
        return lens.map { "No profile for \($0)" } ?? "No lens information"
    }

    /// Guidance when no profile applies automatically (e.g. Sigma or Tamron lenses), or nil.
    public static func noMatchHelp(info: PhotoDevelopInfo?, chosen: String?) -> String? {
        guard chosen == nil, let info, info.lensProfile == nil else { return nil }
        return "Darkslide has no correction profile for this lens, so distortion and corner shading aren't corrected. "
            + "Pick a matching profile below if one fits, or use Vignette and Perspective by hand."
    }
}

/// The lens profiles the pipeline knows (for the manual picker), grouped by maker.
public enum LensProfiles {
    public struct Entry: Hashable, Sendable {
        public let maker: String
        public let model: String
    }

    /// Loaded once from the pipeline's vendored database.
    public static let all: [Entry] = lensProfiles().map { Entry(maker: $0.maker, model: $0.model) }

    public static func grouped(_ entries: [Entry] = all) -> [(maker: String, models: [String])] {
        Dictionary(grouping: entries, by: \.maker).map { ($0.key, $0.value.map(\.model).sorted()) }
            .sorted { $0.maker < $1.maker }
    }
}

/// Decode a JPEG thumbnail (the cache stores JPEG).
enum ThumbnailJPEG {
    static func image(_ data: Data) -> CGImage? {
        guard let src = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        return CGImageSourceCreateImageAtIndex(src, 0, [kCGImageSourceShouldCacheImmediately: true] as CFDictionary)
    }
}
