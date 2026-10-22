import Foundation

/// One process recipe as the app edits and saves it (→ ds-export `ExportRecipe`
/// through `ExportRecipeSpec`).
public struct ExportRecipe: Codable, Equatable, Identifiable, Sendable {
    public enum Format: String, Codable, CaseIterable, Sendable {
        case jpeg, heic, tiff16, png8, png16

        public var title: String {
            switch self {
            case .jpeg: "JPEG"
            case .heic: "HEIC"
            case .tiff16: "TIFF 16-bit"
            case .png8: "PNG 8-bit"
            case .png16: "PNG 16-bit"
            }
        }

        public var fileExtension: String {
            switch self {
            case .jpeg: "jpg"
            case .heic: "heic"
            case .tiff16: "tif"
            case .png8, .png16: "png"
            }
        }

        public var hasQuality: Bool { self == .jpeg || self == .heic }

        /// Google Photos takes the JPEG and HEIC exports.
        public var uploadsToGooglePhotos: Bool { self == .jpeg || self == .heic }
    }

    public enum SizeMode: String, Codable, CaseIterable, Sendable {
        case original, longEdge, shortEdge, megapixels, percent

        public var title: String {
            switch self {
            case .original: "Original"
            case .longEdge: "Long edge (px)"
            case .shortEdge: "Short edge (px)"
            case .megapixels: "Megapixels"
            case .percent: "Percent"
            }
        }
    }

    public enum Space: String, Codable, CaseIterable, Sendable {
        case sRGB, displayP3

        public var title: String { self == .sRGB ? "sRGB" : "Display P3" }
    }

    public var id: UUID
    public var name: String
    public var enabled: Bool
    public var format: Format
    /// 1...100 (JPEG, HEIC).
    public var quality: Int
    public var sizeMode: SizeMode
    /// Pixels, megapixels or percent, depending on `sizeMode`.
    public var sizeValue: Double
    public var allowUpscale: Bool
    public var space: Space
    /// Output sharpening, 0 (off) ... 2.
    public var sharpen: Double
    /// ds-template file name, e.g. `{name}` or `{date:%Y%m%d}_{seq:3}`.
    public var fileNameTemplate: String
    /// `nil`: the session's `Output/` folder.
    public var destination: URL?
    /// A sub-folder of the destination (e.g. `Web`), empty for none.
    public var subfolder: String
    public var includeMetadata: Bool
    public var stripGPS: Bool
    /// Export a straightened or lens-corrected photo at its inscribed size (1:1 sensor
    /// pixels, a few percent smaller) instead of the original size (Lightroom's default).
    public var constrainToSensorPixels: Bool
    /// Convert to this ICC profile and embed it instead of `space`'s (from the soft proof).
    public var outputProfile: RecipeOutputProfile?
    /// AI Upscale: an output larger than the photo starts from the photo upscaled 2× by
    /// the on-device model (then resized to the size); without the model it is resampled.
    public var aiUpscale: Bool
    /// Drawn on each output after resizing and sharpening; nil = none.
    public var watermark: ExportWatermark?
    /// Also upload the outputs to Google Photos, into this album (JPEG and HEIC only); nil = don't.
    public var googlePhotos: GooglePhotosTarget?
    /// Plug-in export steps, run after the watermark in this order.
    public var postProcess: [RecipePostProcess]

    public init(id: UUID = UUID(), name: String, enabled: Bool = true, format: Format = .jpeg, quality: Int = 90,
                sizeMode: SizeMode = .original, sizeValue: Double = 0, allowUpscale: Bool = false,
                space: Space = .sRGB, sharpen: Double = 0, fileNameTemplate: String = "{name}",
                destination: URL? = nil, subfolder: String = "", includeMetadata: Bool = true, stripGPS: Bool = false,
                constrainToSensorPixels: Bool = false, outputProfile: RecipeOutputProfile? = nil, aiUpscale: Bool = false,
                watermark: ExportWatermark? = nil, googlePhotos: GooglePhotosTarget? = nil,
                postProcess: [RecipePostProcess] = []) {
        self.id = id
        self.name = name
        self.enabled = enabled
        self.format = format
        self.quality = quality
        self.sizeMode = sizeMode
        self.sizeValue = sizeValue
        self.allowUpscale = allowUpscale
        self.space = space
        self.sharpen = sharpen
        self.fileNameTemplate = fileNameTemplate
        self.destination = destination
        self.subfolder = subfolder
        self.includeMetadata = includeMetadata
        self.stripGPS = stripGPS
        self.constrainToSensorPixels = constrainToSensorPixels
        self.outputProfile = outputProfile
        self.aiUpscale = aiUpscale
        self.watermark = watermark
        self.googlePhotos = googlePhotos
        self.postProcess = postProcess
    }

    /// Recipes saved before a field existed decode with its default.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(id: try c.decode(UUID.self, forKey: .id), name: try c.decode(String.self, forKey: .name),
                  enabled: try c.decode(Bool.self, forKey: .enabled), format: try c.decode(Format.self, forKey: .format),
                  quality: try c.decode(Int.self, forKey: .quality), sizeMode: try c.decode(SizeMode.self, forKey: .sizeMode),
                  sizeValue: try c.decode(Double.self, forKey: .sizeValue),
                  allowUpscale: try c.decode(Bool.self, forKey: .allowUpscale), space: try c.decode(Space.self, forKey: .space),
                  sharpen: try c.decode(Double.self, forKey: .sharpen),
                  fileNameTemplate: try c.decode(String.self, forKey: .fileNameTemplate),
                  destination: try c.decodeIfPresent(URL.self, forKey: .destination),
                  subfolder: try c.decode(String.self, forKey: .subfolder),
                  includeMetadata: try c.decode(Bool.self, forKey: .includeMetadata),
                  stripGPS: try c.decode(Bool.self, forKey: .stripGPS),
                  constrainToSensorPixels: try c.decodeIfPresent(Bool.self, forKey: .constrainToSensorPixels) ?? false,
                  outputProfile: try c.decodeIfPresent(RecipeOutputProfile.self, forKey: .outputProfile),
                  aiUpscale: try c.decodeIfPresent(Bool.self, forKey: .aiUpscale) ?? false,
                  watermark: try c.decodeIfPresent(ExportWatermark.self, forKey: .watermark),
                  googlePhotos: try c.decodeIfPresent(GooglePhotosTarget.self, forKey: .googlePhotos),
                  postProcess: try c.decodeIfPresent([RecipePostProcess].self, forKey: .postProcess) ?? [])
    }

    /// Name of the default full-resolution recipe (a full develop, e.g. 6000×4000).
    public static let fullSizeJPEGName = "Full-size JPEG"
    /// What preview exports (before ds-pipeline) called it; saved lists are renamed back on load.
    static let previewEraName = "Largest JPEG (preview size)"

    /// The recipes a new user starts with.
    public static let defaults: [ExportRecipe] = [
        ExportRecipe(name: fullSizeJPEGName, format: .jpeg, quality: 92),
        ExportRecipe(name: "Web 2048 JPEG", enabled: false, format: .jpeg, quality: 82, sizeMode: .longEdge,
                     sizeValue: 2048, sharpen: 0.4, subfolder: "Web", stripGPS: true),
        ExportRecipe(name: "HEIC (Display P3)", enabled: false, format: .heic, quality: 85, space: .displayP3,
                     subfolder: "HEIC"),
    ]

    /// Destination directory for this recipe given the batch's default folder.
    public func directory(defaultFolder: URL) -> URL {
        let base = destination ?? defaultFolder
        let sub = subfolder.split(separator: "/").map(String.init)
            .filter { !$0.isEmpty && $0 != "." && $0 != ".." }
        return sub.reduce(base) { $0.appendingPathComponent($1, isDirectory: true) }
    }

    /// A short summary for the recipe list, e.g. "JPEG 92 · 2048 px · sRGB".
    public var summary: String {
        var parts = [format.hasQuality ? "\(format.title) \(quality)" : format.title]
        switch sizeMode {
        case .original: parts.append("original size")
        case .longEdge, .shortEdge: parts.append("\(Int(sizeValue)) px")
        case .megapixels: parts.append("\(sizeValue.formatted()) MP")
        case .percent: parts.append("\(sizeValue.formatted()) %")
        }
        if aiUpscale { parts.append("AI Upscale") }
        parts.append(outputProfile?.name ?? space.title)
        if !includeMetadata { parts.append("no metadata") } else if stripGPS { parts.append("no GPS") }
        if let watermark { parts.append(watermark.summary) }
        if let googlePhotos { parts.append(googlePhotos.summary) }
        parts += postProcess.map(\.name)
        return parts.joined(separator: " · ")
    }
}

/// An ICC profile a recipe converts to and embeds, with the proof's intent and black
/// point compensation (→ ds-ffi `ExportOutputProfile`).
public struct RecipeOutputProfile: Codable, Equatable, Sendable {
    public var name: String
    public var path: String
    public var intent: ProofIntent
    public var blackPointCompensation: Bool

    public init(name: String, path: String, intent: ProofIntent, blackPointCompensation: Bool) {
        self.name = name
        self.path = path
        self.intent = intent
        self.blackPointCompensation = blackPointCompensation
    }
}

/// Saved recipes (UserDefaults, JSON). Unreadable data falls back to the defaults.
@MainActor
public final class ExportRecipeStore {
    public static let key = "exportRecipes"
    let defaults: UserDefaults

    /// `defaults`: the app's own suite (`AppSettings.userDefaults`), so `--snapshot`
    /// and test runs never write the real preferences (M4).
    public init(defaults: UserDefaults) { self.defaults = defaults }

    public func load() -> [ExportRecipe] {
        guard let data = defaults.data(forKey: Self.key),
              var recipes = try? JSONDecoder().decode([ExportRecipe].self, from: data), !recipes.isEmpty
        else { return ExportRecipe.defaults }
        // Exports are full develops again: the preview-era name goes back to "Full-size JPEG".
        for i in recipes.indices where recipes[i].name == ExportRecipe.previewEraName && recipes[i].sizeMode == .original {
            recipes[i].name = ExportRecipe.fullSizeJPEGName
        }
        return recipes
    }

    public func save(_ recipes: [ExportRecipe]) {
        if let data = try? JSONEncoder().encode(recipes) { defaults.set(data, forKey: Self.key) }
    }

    /// Settings › General's artist and copyright (nil when empty), for the exported EXIF.
    public var credit: (artist: String?, copyright: String?) {
        func value(_ key: String) -> String? {
            defaults.string(forKey: key).map { $0.trimmingCharacters(in: .whitespaces) }.flatMap { $0.isEmpty ? nil : $0 }
        }
        return (value(AppSettings.Keys.artist), value(AppSettings.Keys.copyright))
    }
}

/// Where exports go when a recipe has no destination of its own: the session's
/// `Output/` folder when the session folder is a safe place to write, else
/// `~/Pictures/Darkslide Exports/<name>`.
///
/// Never on a card (E-M1): a volume is treated as a card with the card
/// watcher's own test (a local volume with `DCIM/` at its root), or when it is
/// removable or read-only; the volume is inspected off the main thread with a
/// timeout (a hung network share falls back too).
public enum ExportDestination {
    public static func fallbackRoot(home: URL = URL(fileURLWithPath: NSHomeDirectory())) -> URL {
        home.appendingPathComponent("Pictures/Darkslide Exports", isDirectory: true)
    }

    public static func resolve(for session: SessionInfo?, timeout: Double = 5,
                               home: URL = URL(fileURLWithPath: NSHomeDirectory())) async -> URL {
        await resolveExplained(for: session, timeout: timeout, home: home).folder
    }

    /// The default folder and, when it isn't the session's `Output/`, why (L10: a slow
    /// volume never sends exports elsewhere silently).
    public static func resolveExplained(for session: SessionInfo?, timeout: Double = 5,
                                        home: URL = URL(fileURLWithPath: NSHomeDirectory())) async
        -> (folder: URL, note: String?) {
        await resolveExplained(for: session, timeout: timeout, home: home) { isSafeExportFolder($0) }
    }

    static func resolveExplained(for session: SessionInfo?, timeout: Double, home: URL,
                                 check: @escaping @Sendable (URL) -> Bool) async -> (folder: URL, note: String?) {
        let fallback = fallbackRoot(home: home)
        guard let session else { return (fallback, nil) }
        let folder = session.url
        let checked = await blockingWithTimeout(timeout) { check(folder) }
        if checked == true { return (folder.appendingPathComponent("Output", isDirectory: true), nil) }
        let other = fallback.appendingPathComponent(session.name, isDirectory: true)
        let note = checked == nil
            ? "The session's folder didn't answer in time, so exports go to \(other.path). Choose another folder if you like."
            : "The session's folder is on a card or can't be written, so exports go to \(other.path)."
        return (other, note)
    }

    /// Blocking: may stat the volume. Writable, not read-only, not removable, and
    /// not on a volume that looks like a camera card (`DCIM/` at its root).
    static func isSafeExportFolder(_ folder: URL, fm: FileManager = .default) -> Bool {
        let keys: Set<URLResourceKey> = [.volumeIsReadOnlyKey, .volumeIsRemovableKey, .volumeIsLocalKey, .volumeURLKey]
        guard let v = try? folder.resourceValues(forKeys: keys) else { return false }
        if v.volumeIsReadOnly == true || v.volumeIsRemovable == true { return false }
        if let root = v.volume, looksLikeCard(root, isLocal: v.volumeIsLocal ?? false, fm: fm) { return false }
        return fm.isWritableFile(atPath: folder.path)
    }

    /// The card watcher's test (`DCIMDetector`): local, with a `DCIM/` directory at the root.
    static func looksLikeCard(_ volumeRoot: URL, isLocal: Bool, fm: FileManager = .default) -> Bool {
        var dir: ObjCBool = false
        return isLocal && fm.fileExists(atPath: volumeRoot.appendingPathComponent("DCIM").path, isDirectory: &dir)
            && dir.boolValue
    }
}
