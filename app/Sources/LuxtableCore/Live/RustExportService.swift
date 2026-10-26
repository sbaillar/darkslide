import CoreGraphics
import LuxtableAI
import LuxtableFFI
import Foundation

/// Export through lt-export (`export_batch`): recipes → files, with the ImageIO
/// encoder for HEIC (and JPEG when `hardwareJPEG`), progress and cancel.
///
/// RAWs are developed by lt-pipeline with each photo's edits (crop, straighten,
/// turns and every adjustment), at each recipe's size and output sharpening, on
/// the app's shared GPU renderer at background priority (the viewer's renders go
/// first). JPEG, HEIF, PNG and TIFF originals are decoded at full size by
/// `ImageIOEncoder`.
public struct RustExportService: ExportService {
    /// Encode JPEG with ImageIO instead of mozjpeg. Off by default: on the A9
    /// previews mozjpeg is about twice as fast at the same size and PSNR
    /// (`Exp6LiveExportTests.encoderBenchmark`: q90 9.0 ms, 544 KB, 40.6 dB vs
    /// ImageIO q80 17.7 ms, 547 KB, 41.4 dB per image). HEIC always uses ImageIO.
    public var hardwareJPEG: Bool
    /// AI mask bitmaps (Apple Vision) for layers with AI shapes, the viewer's provider.
    /// Gathered once per photo before the export starts; without it those shapes are empty.
    public var maskProvider: DevelopRenderer.MaskProvider?
    /// Depth maps for the lens blur (nil: `DepthWiring.provider`).
    public var depthProvider: DepthProvider?
    /// Per-person masks (`MaskShape.aiPerson`); missing ones render empty.
    public var personMaskProvider: PersonMaskProvider?
    /// Before photos with removals export: nil when they fill as the viewer shows them,
    /// else why not (`RemovalModel.exportProblem`, which waits for LaMa on disk).
    public var removalCheck: (@Sendable () async -> String?)?
    let encoder: ImageIOEncoder
    /// Text watermarks (Core Text).
    let text = CoreTextWatermarkRenderer()

    public init(hardwareJPEG: Bool = false, encoder: ImageIOEncoder = ImageIOEncoder()) {
        self.hardwareJPEG = hardwareJPEG
        self.encoder = encoder
    }

    static func spec(_ r: ExportRecipe, defaultFolder: URL) -> ExportRecipeSpec {
        let q = UInt8(clamping: r.quality)
        let format: ExportFileFormat = switch r.format {
        case .jpeg: .jpeg(quality: q)
        case .heic: .heic(quality: q)
        case .tiff16: .tiff16
        case .png8: .png8
        case .png16: .png16
        }
        let v = r.sizeValue
        let size: ExportSize = switch r.sizeMode {
        case .original: .original
        case .longEdge: .longEdge(pixels: UInt32(clamping: Int(v.rounded())))
        case .shortEdge: .shortEdge(pixels: UInt32(clamping: Int(v.rounded())))
        case .megapixels: .megapixels(megapixels: Float(v))
        case .percent: .percent(percent: Float(v))
        }
        return ExportRecipeSpec(
            name: r.name, format: format, size: size, allowUpscale: r.allowUpscale,
            space: r.space == .sRGB ? .srgb : .displayP3, sharpen: Float(r.sharpen),
            fileNameTemplate: r.fileNameTemplate,
            destination: r.directory(defaultFolder: defaultFolder).path,
            includeExif: r.includeMetadata, stripGps: r.stripGPS, constrainToSensorPixels: r.constrainToSensorPixels,
            outputProfile: r.outputProfile.map {
                ExportOutputProfile(path: $0.path, intent: $0.intent.ffi, blackPointCompensation: $0.blackPointCompensation)
            },
            aiUpscale: r.aiUpscale,
            watermark: r.watermark.map(WatermarkPreview.spec),
            postProcess: r.postProcess.map(\.spec), labelTags: r.labelTags)
    }

    public func validate(_ recipes: [ExportRecipe], defaultFolder: URL) -> String? {
        if recipes.isEmpty { return "Choose at least one recipe." }
        do {
            if let problem = try validateExportRecipes(recipes: recipes.map { Self.spec($0, defaultFolder: defaultFolder) }) {
                return Self.plain(problem)
            }
        } catch {
            return "\(error)"
        }
        // A recipe made on another Mac may name a font this one lacks (Core Text would
        // silently use the system font).
        for r in recipes {
            if let w = r.watermark, w.kind == .text, !w.fontFamily.isEmpty,
               !CoreTextWatermarkRenderer.isInstalled(w.fontFamily) {
                return "\(r.name): the watermark font \(w.fontFamily) isn't installed on this Mac."
            }
        }
        return nil
    }

    public func previewFileName(template: String, sample: ExportFileNameSample, seq: Int, ext: String) -> String? {
        try? exportFileNamePreview(template: template, name: sample.name, seq: UInt64(max(0, seq)), camera: sample.camera,
                                   captureTime: sample.captureTime, extension: ext)
    }

    /// Reads the file's metadata off the calling actor (one small read per sheet).
    public func nameSample(for url: URL) async -> ExportFileNameSample? {
        let path = url.path
        return await blockingWithTimeout(2) {
            (try? exportNameSample(path: path)).map {
                ExportFileNameSample(name: $0.name, camera: $0.camera, captureTime: $0.captureTime,
                                     artist: $0.artist, copyright: $0.copyright)
            }
        } ?? nil
    }

    /// [map] A photo's position for the export (it wins over the file's GPS; `strip_gps` still removes it).
    static func gps(_ g: GeoPoint) -> ExportGps {
        ExportGps(latitude: g.latitude, longitude: g.longitude, altitudeM: g.altitude)
    }

    public func watermarkPreview(_ pixels: WatermarkPreviewPixels, watermark: ExportWatermark,
                                 sample: ExportFileNameSample) -> WatermarkPreviewResult {
        do {
            let out = try LuxtableFFI.watermarkPreview(
                width: UInt32(pixels.width), height: UInt32(pixels.height), rgba: pixels.rgba,
                watermark: WatermarkPreview.spec(watermark),
                sample: WatermarkSample(name: sample.name, copyright: sample.copyright, artist: sample.artist,
                                        captureTime: sample.captureTime),
                text: text)
            let image = WatermarkPreview.image(width: pixels.width, height: pixels.height, rgba: out.rgba)
            let blank = out.text.map { $0.trimmingCharacters(in: .whitespaces).isEmpty } ?? false
            return WatermarkPreviewResult(image: image, message: blank ? WatermarkPreview.blankMessage : nil)
        } catch {
            let message = (error as? DsError).map { RenderError($0).message } ?? "\(error)"
            return WatermarkPreviewResult(image: nil, message: message)
        }
    }

    /// "invalid argument: Web: …" → "Web: …".
    static func plain(_ message: String) -> String {
        message.hasPrefix("invalid argument: ") ? String(message.dropFirst("invalid argument: ".count)) : message
    }

    public func export(_ job: ExportJob, progress: @escaping @Sendable (ExportProgressUpdate) -> Void) async throws -> ExportOutcome {
        let recipes = job.recipes
        let specs = recipes.map { Self.spec($0, defaultFolder: job.defaultFolder) }
        let byIndex = Dictionary(uniqueKeysWithValues: job.items.enumerated().map { (UInt64($0.offset + 1), $0.element) })
        // M9: a photo whose AI mask can't be computed (e.g. the sky model is missing) is
        // not exported without that layer: it fails, and says why.
        let (masks, missing) = await gatherMasks(byIndex)
        // A photo whose removals would fill differently than previewed fails, and says why.
        let removalFailures = await removalFailures(byIndex)
        try Task.checkCancellation() // cancelled while waiting for the removal model
        let sources = try byIndex.sorted { $0.key < $1.key }
            .filter { missing[$0.key] == nil && removalFailures[$0.key] == nil }.map { id, item in
            ExportSource(id: id, path: item.url.path, seq: UInt64(item.seq), gps: item.location.map(Self.gps), // [map]
                         artist: job.artist, copyright: job.copyright,
                         adjustmentsJson: try item.edit.map { try AdjustmentsCodec.encode($0) }, keywords: item.keywords,
                         metadata: item.metadata.isEmpty ? nil : RustSessionStore.record(item.metadata),
                         rating: item.rating.map { UInt8(clamping: $0) }, label: RustSessionStore.label(item.colorTag),
                         pick: RustSessionStore.flag(item.pick))
        }
        let maskFailures = missing.sorted { $0.key < $1.key }.compactMap { id, targets in
            byIndex[id].map { Self.maskFailure($0, targets: targets) }
        } + removalFailures.filter { missing[$0.key] == nil }.sorted { $0.key < $1.key }.map(\.value)
        if sources.isEmpty {
            return ExportOutcome(written: [], failures: maskFailures, cancelled: false, jpegEncoder: "")
        }
        let stream = ProgressStream()
        let listen = Task {
            for await e in stream.events {
                switch e.kind {
                case "export": progress(ExportProgressUpdate(done: Int(e.completed), total: Int(e.total)))
                case "export.metadata":
                    progress(ExportProgressUpdate(done: Int(e.completed), total: Int(e.total), readingMetadata: true))
                default: break
                }
            }
        }
        defer {
            stream.finish()
            listen.cancel()
        }
        let token = try CancelToken()
        let report: ExportReport
        do {
            report = try await withTaskCancellationHandler {
                try await exportBatch(sources: sources, recipes: specs, encoder: encoder, hardwareJpeg: hardwareJPEG,
                                      listener: stream, token: token, masks: masks, text: text)
            } onCancel: {
                try? token.cancel()
                masks?.depth.cancel() // a render waiting for a depth map stops waiting
            }
        } catch let e as DsError {
            if case .Cancelled = e { throw CancellationError() }
            throw RenderError(e)
        }
        func label(_ source: UInt64?, _ recipe: UInt32?) -> String {
            let name = source.flatMap { byIndex[$0] }?.url.deletingPathExtension().lastPathComponent
            let rec = recipe.flatMap { Int($0) < recipes.count ? recipes[Int($0)].name : nil }
            return [name, rec].compactMap { $0 }.joined(separator: " · ")
        }
        return ExportOutcome(
            written: report.written.compactMap { w in
                guard let item = byIndex[w.sourceId] else { return nil }
                return ExportedOutput(photoID: item.photoID, recipeName: label(nil, w.recipe),
                                      url: URL(fileURLWithPath: w.path), width: Int(w.width), height: Int(w.height),
                                      bytes: Int64(w.bytes))
            },
            failures: maskFailures + report.failed.compactMap { f in
                guard let message = ExportDepth.failureMessage(f.message, gaveUp: f.sourceId.flatMap { masks?.depth.gaveUp($0) })
                else { return nil }
                let where_ = label(f.sourceId, f.recipe)
                return where_.isEmpty ? message : "\(where_): \(message)"
            },
            cancelled: report.cancelled,
            renderer: report.renderer,
            jpegEncoder: report.jpegEncoder,
            watermarkSkipped: report.written.count { $0.watermarkSkipped })
    }
}

extension RustExportService {
    /// The AI masks every photo's edit needs, keyed by export source id (nil when none do).
    func exportMasks(_ items: [UInt64: ExportJobItem]) async -> ExportMasks? {
        await gatherMasks(items).masks
    }

    /// The AI masks every photo's edit needs, and per source id the targets the provider
    /// couldn't compute (a photo with no sky gets an empty mask, not a missing one). With
    /// no provider nothing is reported missing (the shapes render empty, as before).
    func gatherMasks(_ items: [UInt64: ExportJobItem]) async -> (masks: ExportMasks?, missing: [UInt64: Set<AITarget>]) {
        let depths = exportDepth(items)
        guard let provider = maskProvider else {
            return (depths.map { ExportMasks([:], depth: $0) }, [:])
        }
        let persons = await gatherPersonMasks(items)
        var out: [UInt64: [AITarget: VisionMask]] = [:]
        var missing: [UInt64: Set<AITarget>] = [:]
        for (id, item) in items {
            guard let edit = item.edit else { continue }
            let targets = DevelopRenderer.aiTargets(in: edit)
            guard !targets.isEmpty || !DevelopRenderer.personKeys(in: edit).isEmpty else { continue }
            let masks = targets.isEmpty ? [:] : await provider(item.photoID, targets)
            out[id, default: [:]].merge(masks) { _, new in new }
            var absent = targets.subtracting(masks.keys)
            // A per-person mask the people model couldn't make (or a person no longer
            // found) is reported like a whole-image one, not rendered empty.
            if personMaskProvider != nil {
                let got = persons[id] ?? [:]
                for key in DevelopRenderer.personKeys(in: edit) where got[key] == nil { absent.insert(key.target) }
            }
            if !absent.isEmpty { missing[id] = absent }
        }
        let any = !out.isEmpty || depths != nil
        return (any ? ExportMasks(out, persons: persons, depth: depths ?? .none) : nil, missing)
    }

    /// The depth maps of the photos with a depth lens blur, each made when the render asks
    /// for it (nil when no photo needs one or nothing makes maps).
    func exportDepth(_ items: [UInt64: ExportJobItem]) -> ExportDepth? {
        guard let depth = depthProvider ?? DepthWiring.provider else { return nil }
        let photos = items.compactMapValues { $0.edit?.effects.lensBlur.needsDepth == true ? $0.photoID : nil }
        return photos.isEmpty ? nil : ExportDepth(photos: photos, provider: depth)
    }

    /// Per source id with removals, why it isn't exported (empty when they fill as
    /// previewed, or nothing has removals).
    func removalFailures(_ items: [UInt64: ExportJobItem]) async -> [UInt64: String] {
        let withRemovals = items.filter { $0.value.edit?.removals.isEmpty == false }
        guard !withRemovals.isEmpty, let removalCheck, let why = await removalCheck() else { return [:] }
        return withRemovals.mapValues { item in
            "\(item.url.deletingPathExtension().lastPathComponent): not exported. Its removals can't be filled as previewed: \(why)."
        }
    }

    /// "DSC0001: …": why a photo with an AI mask layer wasn't exported.
    static func maskFailure(_ item: ExportJobItem, targets: Set<AITarget>) -> String {
        let name = item.url.deletingPathExtension().lastPathComponent
        let what = targets.map(\.rawValue).sorted().joined(separator: " and ")
        let why = targets.contains(.sky)
            ? "the sky model isn't available on this Mac. Open the Layers panel to download it, then export again"
            : targets.allSatisfy(PeopleTargets.isPeople)
            ? "the person it was made for isn't found in the photo any more, or Vision couldn't compute it on this Mac"
            : "Vision couldn't compute it on this Mac"
        return "\(name): not exported. Its \(what) mask couldn't be made: \(why)."
    }
}

/// The depth maps of an export's photos, made one photo at a time as the render asks for
/// them (on the export's thread, which waits: a blur exports as previewed; a photo whose
/// map can't be made exports without it). Only the last few are kept, so an export of
/// hundreds of blurred photos never holds all their maps.
final class ExportDepth: @unchecked Sendable {
    static let keep = 4
    static let none = ExportDepth(photos: [:]) { _, _ in nil }

    private let photos: [UInt64: PhotoID]
    private let provider: DepthProvider
    /// How long one map may take before the photo fails instead (it waits for its turn
    /// behind other urgent maps, then is made: seconds, normally).
    private let timeout: Duration
    private let lock = NSLock()
    private var recent: [(id: UInt64, map: DepthResult?)] = []
    private var cancelled = false
    private var gaveUp: [UInt64: GaveUp] = [:]

    /// Why a render stopped waiting for a photo's map.
    enum GaveUp: Equatable {
        case cancelled, tooSlow
    }

    init(photos: [UInt64: PhotoID], timeout: Duration = .seconds(120), provider: @escaping DepthProvider) {
        self.photos = photos
        self.timeout = timeout
        self.provider = provider
    }

    /// The export was cancelled: a render waiting for a map stops waiting.
    func cancel() { lock.withLock { cancelled = true } }

    /// Why the export stopped waiting for source `id`'s map (nil: it didn't).
    func gaveUp(_ id: UInt64) -> GaveUp? { lock.withLock { gaveUp[id] } }

    /// A failure's message as the user reads it: a map that took too long says so, and
    /// a photo whose wait was cancelled isn't a failure (the export was cancelled).
    static func failureMessage(_ message: String, gaveUp: GaveUp?) -> String? {
        switch gaveUp {
        case .cancelled: nil
        case .tooSlow: "not exported: its lens blur's depth map took too long"
        case nil: message
        }
    }

    private var isCancelled: Bool { lock.withLock { cancelled } }

    /// Fixed maps by source id (tests).
    convenience init(_ maps: [UInt64: DepthResult]) {
        self.init(photos: maps.mapValues { _ in PhotoID(0) }) { _, _ in nil }
        recent = maps.map { (id: $0.key, map: $0.value) }
    }

    /// Source id `id`'s map, made now if needed (blocks the calling thread: never call it
    /// from the main thread or a Swift concurrency thread).
    func map(_ id: UInt64) -> DepthResult? {
        guard let photo = photos[id] else { return nil }
        if let hit = lock.withLock({ recent.first { $0.id == id } }) { return hit.map }
        let provider = provider
        guard let got = Self.wait(timeout: timeout, giveUp: { [weak self] in self?.isCancelled ?? true }, {
            await provider(photo, true)
        }) else { // cancelled or too slow: not remembered, the photo fails
            lock.withLock { gaveUp[id] = cancelled ? .cancelled : .tooSlow }
            return nil
        }
        lock.withLock {
            recent.removeAll { $0.id == id }
            recent.append((id: id, map: got))
            if recent.count > Self.keep { recent.removeFirst() }
        }
        return got
    }

    private final class Box<T>: @unchecked Sendable {
        var value: T?
    }

    /// `body`'s result, waited for on this thread; nil when `giveUp` turns true or
    /// `timeout` passes first (the work is then cancelled).
    static func wait<T: Sendable>(timeout: Duration, giveUp: @escaping () -> Bool,
                                  _ body: @escaping @Sendable () async -> T) -> T? {
        let done = DispatchSemaphore(value: 0)
        let box = Box<T>()
        let task = Task.detached {
            box.value = await body()
            done.signal()
        }
        let deadline = ContinuousClock.now + timeout
        while done.wait(timeout: .now() + 0.05) == .timedOut {
            if giveUp() || ContinuousClock.now >= deadline {
                task.cancel()
                return nil
            }
        }
        return box.value
    }
}

/// AI masks of an export's photos (lt-ffi `ExportMaskSource`), in the viewer's format.
final class ExportMasks: ExportMaskSource, @unchecked Sendable {
    let masks: [UInt64: StaticMasks]
    let depth: ExportDepth

    init(_ masks: [UInt64: [AITarget: VisionMask]], persons: [UInt64: [PersonMaskKey: VisionMask]] = [:],
         depth: ExportDepth = .none) {
        var all: [UInt64: StaticMasks] = [:]
        for id in Set(masks.keys).union(persons.keys) {
            all[id] = StaticMasks(masks[id] ?? [:], persons: persons[id] ?? [:])
        }
        self.masks = all
        self.depth = depth
    }

    func depthMap(sourceId: UInt64) -> DevelopMask? {
        depth.map(sourceId).map { DevelopMask($0.depth) }
    }

    /// A layer's mask, else (the subject) the depth map's matte: the depth blur's auto focus.
    func aiMask(sourceId: UInt64, target: DevelopAiTarget) -> DevelopMask? {
        if let m = masks[sourceId]?.aiMask(target: target) { return m }
        guard target == .subject else { return nil }
        return depth.map(sourceId)?.subject.map(DevelopMask.init)
    }

    func aiPersonMask(sourceId: UInt64, target: DevelopAiTarget, person: UInt32) -> DevelopMask? {
        masks[sourceId]?.aiPersonMask(target: target, person: person)
    }
}

/// Export in demo mode: nothing is written (the demo has no files).
public struct UnavailableExportService: ExportService {
    public init() {}
    public func validate(_ recipes: [ExportRecipe], defaultFolder: URL) -> String? {
        "Export works in live mode (run without --demo)."
    }

    public func previewFileName(template: String, sample: ExportFileNameSample, seq: Int, ext: String) -> String? { nil }

    public func export(_ job: ExportJob, progress: @escaping @Sendable (ExportProgressUpdate) -> Void) async throws -> ExportOutcome {
        throw RenderError(.Unsupported(message: "Export works in live mode (run without --demo)."))
    }
}
