import CoreGraphics
import Foundation
import Observation

/// The photo the recipe editor previews watermarks on, and how to load its image.
public struct WatermarkPreviewSource: Sendable {
    public var url: URL
    public var load: @Sendable () async -> CGImage?

    public init(url: URL, load: @escaping @Sendable () async -> CGImage?) {
        self.url = url
        self.load = load
    }
}

/// The Export sheet: the selected photos × enabled recipes, run with progress and
/// cancel, then a result list with "Show in Finder".
@MainActor @Observable
public final class ExportModel {
    public enum Phase: Equatable {
        case editing
        case running(done: Int, total: Int)
        case finished(ExportOutcome)
        case failed(String)
    }

    public let items: [ExportJobItem]
    /// Selected videos left out (they aren't exported).
    public let skippedVideos: Int
    public private(set) var defaultFolder: URL
    /// False while the default folder is being checked off the main thread.
    public private(set) var destinationReady: Bool
    public var recipes: [ExportRecipe] { didSet { store.save(recipes) } }
    public var selectedRecipeID: UUID?
    public private(set) var phase: Phase = .editing
    /// True while the originals' metadata is read (before the first render).
    public private(set) var readingMetadata = false
    public private(set) var startedAt: Date?
    /// `{name}`/`{camera}`/`{date}` of the first photo, as the export reads them.
    public private(set) var nameSample: ExportFileNameSample?

    /// The watermark preview's photo (sRGB RGBA8, converted once) and its
    /// `{filename}`/`{year}`/EXIF credits; loaded the first time a watermark is previewed.
    public private(set) var previewPixels: WatermarkPreviewPixels?
    public private(set) var previewSample: ExportFileNameSample?
    @ObservationIgnored let previewSource: WatermarkPreviewSource?
    @ObservationIgnored private var previewLoading: Task<Void, Never>?
    /// The last preview drawn and the watermark it shows (the editor only reads it).
    public private(set) var watermarkPreview: (watermark: ExportWatermark, result: WatermarkPreviewResult)?
    /// Quiet time after an edit before the preview is drawn (slider drags draw once they pause).
    @ObservationIgnored var previewDebounce: Duration = .milliseconds(80)

    /// Why the default folder isn't the session's `Output/` (L10), or nil.
    public private(set) var destinationNote: String?
    /// Why AI Upscale can't run in this build (nil: it can).
    public var aiUpscaleUnavailable: String?
    @ObservationIgnored let service: any ExportService
    @ObservationIgnored let store: ExportRecipeStore
    /// Uploads for recipes with a Google Photos destination (nil: not available).
    @ObservationIgnored let googlePhotos: (any GooglePhotosService)?
    /// Whether the Google Photos integration is on (Settings ▸ Plugins), asked before
    /// each check and each upload: turned off mid-run, the next recipe isn't uploaded.
    @ObservationIgnored let googlePhotosOn: @MainActor () -> Bool
    /// The Google Photos uploads of the last run, one per recipe that has one.
    public private(set) var googleUploads: [GooglePhotosUpload] = []
    /// While the exported files are uploaded to Google Photos.
    public private(set) var uploading: GooglePhotosProgress?
    /// This app's Google Photos albums, once `loadGoogleAlbums()` has listed them.
    public private(set) var googleAlbums: [GooglePhotosAlbum] = []
    public private(set) var googleAlbumsMessage: String?
    @ObservationIgnored var reveal: ([URL]) -> Void = { PlatformWorkspace.reveal($0) }
    @ObservationIgnored private var task: Task<Void, Never>?
    @ObservationIgnored private var preparing: Task<Void, Never>?

    /// `defaultFolder`: nil while it is resolved by `prepare(session:)`.
    public init(items: [ExportJobItem], defaultFolder: URL?, service: any ExportService, store: ExportRecipeStore,
                skippedVideos: Int = 0, previewSource: WatermarkPreviewSource? = nil,
                googlePhotos: (any GooglePhotosService)? = nil, googlePhotosOn: @escaping @MainActor () -> Bool = { true }) {
        self.items = items
        self.googlePhotos = googlePhotos
        self.googlePhotosOn = googlePhotosOn
        self.previewSource = previewSource
        self.skippedVideos = skippedVideos
        self.defaultFolder = defaultFolder ?? ExportDestination.fallbackRoot()
        destinationReady = defaultFolder != nil
        self.service = service
        self.store = store
        recipes = store.load()
        selectedRecipeID = recipes.first?.id
    }

    /// Off the main thread: the default folder (never a card, E-M1; only when not
    /// given to `init`), then the first photo's name sample (E-L2). The watermark
    /// preview's photo is loaded only when a watermark is previewed.
    public func prepare(session: SessionInfo?, timeout: Double = 5) {
        let service = service, first = items.first?.url, needsFolder = !destinationReady
        preparing = Task { [weak self] in
            if needsFolder {
                let (folder, note) = await ExportDestination.resolveExplained(for: session, timeout: timeout)
                self?.defaultFolder = folder
                self?.destinationNote = note
                self?.destinationReady = true
            }
            if let first {
                let sample = await service.nameSample(for: first)
                self?.nameSample = sample
            }
        }
    }

    /// Wait for `prepare` (tests).
    func prepared() async { await preparing?.value }

    public var enabledRecipes: [ExportRecipe] { recipes.filter(\.enabled) }

    /// Why Export is disabled, or nil.
    public var problem: String? {
        if items.isEmpty {
            return skippedVideos > 0 ? "Videos aren't exported. Select photos to export." : "Select photos to export."
        }
        if !destinationReady { return "Checking the export folder…" }
        if enabledRecipes.isEmpty { return "Turn on at least one recipe." }
        if let p = googlePhotosProblem { return p }
        return service.validate(enabledRecipes, defaultFolder: defaultFolder)
    }

    public var canStart: Bool {
        if case .running = phase { return false }
        return problem == nil
    }

    public var isRunning: Bool {
        if case .running = phase { return true }
        return false
    }

    /// Outputs this run will write.
    public var outputCount: Int { items.count * enabledRecipes.count }

    /// File name of the first photo for `recipe` (live preview under the template):
    /// its real capture time and camera once `prepare` has read them (E-L2).
    public func sampleFileName(_ recipe: ExportRecipe) -> String? {
        guard let first = items.first else { return nil }
        let sample = nameSample ?? ExportFileNameSample(name: first.url.deletingPathExtension().lastPathComponent)
        return service.previewFileName(template: recipe.fileNameTemplate, sample: sample, seq: first.seq,
                                       ext: recipe.format.fileExtension)
    }

    /// The recipe's AI Upscale toggle explained.
    public var aiUpscaleHelp: String {
        let what = "Sizes larger than the photo start from it upscaled 2× by on-device AI."
        return aiUpscaleUnavailable.map { "\($0) Larger sizes are resampled instead." } ?? what
    }

    /// "N videos aren't exported" (nil when none were selected).
    public var skippedVideosNote: String? {
        guard skippedVideos > 0 else { return nil }
        return skippedVideos == 1 ? "1 video isn't exported (videos stay as they are)."
            : "\(skippedVideos) videos aren't exported (videos stay as they are)."
    }

    /// The stored preview for `watermark` (nil while it is drawn, or for another watermark).
    public func watermarkPreview(for watermark: ExportWatermark?) -> WatermarkPreviewResult? {
        guard let watermark, let p = watermarkPreview, p.watermark == watermark else { return nil }
        return p.result
    }

    /// Draw `recipe`'s watermark on the preview photo, off the main thread, after
    /// `previewDebounce` of quiet: the editor runs it as `.task(id:)` of the watermark,
    /// so an edit cancels the draw in flight. Loads the preview photo the first time.
    public func updateWatermarkPreview(_ recipe: ExportRecipe) async {
        guard let watermark = recipe.watermark else { return }
        try? await Task.sleep(for: previewDebounce)
        if Task.isCancelled { return }
        await loadPreviewPhoto()
        guard !Task.isCancelled, let pixels = previewPixels else { return }
        // The credits the export uses: Settings' first, else the photo's own EXIF.
        let credit = store.credit
        var sample = previewSample ?? ExportFileNameSample(name: "")
        sample.artist = credit.artist ?? sample.artist
        sample.copyright = credit.copyright ?? sample.copyright
        let service = service
        let result = await Task.detached(priority: .userInitiated) {
            service.watermarkPreview(pixels, watermark: watermark, sample: sample)
        }.value
        if Task.isCancelled { return }
        watermarkPreview = (watermark, result)
    }

    /// The preview photo and its sample, loaded once (concurrent callers share the load).
    func loadPreviewPhoto() async {
        guard let source = previewSource else { return }
        if previewLoading == nil {
            let service = service
            previewLoading = Task { [weak self] in
                let image = await source.load()
                let pixels = await Task.detached { image.flatMap { WatermarkPreview.pixels($0) } }.value
                let sample = await service.nameSample(for: source.url)
                    ?? ExportFileNameSample(name: source.url.deletingPathExtension().lastPathComponent)
                self?.previewPixels = pixels
                self?.previewSample = sample
            }
        }
        await previewLoading?.value
    }

    // MARK: Recipes

    public var selectedRecipe: ExportRecipe? {
        get { recipes.first { $0.id == selectedRecipeID } }
        set {
            guard let newValue, let i = recipes.firstIndex(where: { $0.id == newValue.id }) else { return }
            recipes[i] = newValue
        }
    }

    public func addRecipe() {
        let r = ExportRecipe(name: uniqueName("New Recipe"))
        recipes.append(r)
        selectedRecipeID = r.id
    }

    public func duplicateSelected() {
        guard var r = selectedRecipe else { return }
        r.id = UUID()
        r.name = uniqueName(r.name + " copy")
        recipes.append(r)
        selectedRecipeID = r.id
    }

    public func removeRecipe(_ id: UUID) {
        guard recipes.count > 1, let i = recipes.firstIndex(where: { $0.id == id }) else { return }
        recipes.remove(at: i)
        if selectedRecipeID == id { selectedRecipeID = recipes[min(i, recipes.count - 1)].id }
    }

    private func uniqueName(_ base: String) -> String {
        let names = Set(recipes.map(\.name))
        guard names.contains(base) else { return base }
        return (2...).lazy.map { "\(base) \($0)" }.first { !names.contains($0) }!
    }

    // MARK: Run

    public func start() {
        guard canStart else { return }
        let credit = store.credit
        let job = ExportJob(items: items, recipes: enabledRecipes, defaultFolder: defaultFolder,
                            artist: credit.artist, copyright: credit.copyright)
        phase = .running(done: 0, total: items.count)
        readingMetadata = true
        startedAt = Date()
        let service = service
        // M4: temps an earlier, interrupted export left in these folders go first; the
        // folders are remembered so a quit mid-run is swept at the next launch.
        let folders = Array(Set(enabledRecipes.map { $0.directory(defaultFolder: defaultFolder) }))
        ExportPartials.remember(folders, defaults: store.defaults)
        // Progress arrives on Rust's delivery thread; only the newest update matters.
        let (updates, sink) = AsyncStream.makeStream(of: ExportProgressUpdate.self, bufferingPolicy: .bufferingNewest(1))
        task = Task { [weak self] in
            let pump = Task { [weak self] in
                for await u in updates { self?.progressed(u) }
            }
            defer {
                sink.finish()
                pump.cancel()
            }
            do {
                await Task.detached(priority: .utility) { folders.forEach { ExportPartials.sweep($0) } }.value
                let outcome = try await service.export(job) { (u: ExportProgressUpdate) in sink.yield(u) }
                self?.readingMetadata = false
                await self?.uploadToGooglePhotos(outcome, job: job)
                self?.phase = .finished(outcome)
            } catch is CancellationError {
                self?.readingMetadata = false
                self?.phase = .finished(ExportOutcome(written: [], failures: [], cancelled: true, jpegEncoder: ""))
            } catch {
                self?.readingMetadata = false
                self?.phase = .failed((error as? LocalizedError)?.errorDescription ?? "\(error)")
            }
        }
    }

    private func progressed(_ u: ExportProgressUpdate) {
        guard case .running = phase else { return }
        readingMetadata = u.readingMetadata
        phase = .running(done: u.readingMetadata ? 0 : u.done, total: u.total)
        metadataDone = u.readingMetadata ? u.done : 0
    }

    /// Photos whose metadata was read so far (while `readingMetadata`).
    public private(set) var metadataDone = 0

    /// The running line: "Reading metadata 12 of 300…" then "Exporting 3 of 300…".
    public var runningText: String? {
        guard case .running(let done, let total) = phase else { return nil }
        if let u = uploading {
            return u.adding ? "Adding to the Google Photos album…" : "Uploading \(min(u.done + 1, u.total)) of \(u.total) to Google Photos…"
        }
        return readingMetadata ? "Reading metadata \(metadataDone) of \(total)…" : "Exporting \(done) of \(total)…"
    }

    public func cancel() { task?.cancel() }

    /// Stop a running export for quitting (M4): cancel, then wait at most `timeout` for
    /// the batch to stop (lt-export deletes its temp for the photo in flight; a temp
    /// left by a hard stop is swept at the next launch). Returns whether it stopped.
    @discardableResult
    public func stopForQuit(timeout: Duration = .seconds(5)) async -> Bool {
        guard let task else { return true }
        task.cancel()
        return await withCheckedContinuation { cont in
            let once = ExportOnce()
            Task { await task.value; if once.claim() { cont.resume(returning: true) } }
            Task {
                try? await Task.sleep(for: timeout)
                if once.claim() { cont.resume(returning: false) }
            }
        }
    }

    /// Wait for the run to end (tests).
    func waitUntilDone() async { await task?.value }

    public func showInFinder(_ urls: [URL]) {
        guard !urls.isEmpty else { return }
        reveal(urls)
    }

    /// Back to the recipes after a run (to export again).
    public func reset() {
        if case .running = phase { return }
        phase = .editing
        googleUploads = []
    }

    /// "12 files · 38.4 MB" for a finished run.
    public static func summary(_ o: ExportOutcome) -> String {
        let bytes = o.written.reduce(Int64(0)) { $0 + $1.bytes }
        let size = ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
        var s = "\(o.written.count) \(o.written.count == 1 ? "file" : "files") · \(size)"
        if !o.failures.isEmpty { s += " · \(o.failures.count) failed" }
        if o.watermarkSkipped > 0 {
            s += " · \(o.watermarkSkipped) without watermark (blank text)"
        }
        if o.cancelled { s += " · cancelled" }
        return s
    }

}

// MARK: Google Photos

extension ExportModel {
    /// Why the enabled recipes' Google Photos uploads can't run, or nil.
    var googlePhotosProblem: String? {
        for r in enabledRecipes {
            guard let target = r.googlePhotos else { continue }
            // Never a silent skip: the recipe keeps its album; the user turns it back on or
            // turns the upload off in the recipe.
            guard googlePhotosOn() else { return Self.googlePhotosOffProblem(r.name) }
            guard let googlePhotos, googlePhotos.isSignedIn else {
                return "Sign in to Google Photos in Settings ▸ Google Photos to upload with “\(r.name)”."
            }
            if !r.format.uploadsToGooglePhotos { return "“\(r.name)” uploads to Google Photos: choose JPEG or HEIC." }
            if target.albumID == nil && target.albumTitle.trimmingCharacters(in: .whitespaces).isEmpty {
                return "Name the Google Photos album for “\(r.name)”."
            }
        }
        return nil
    }

    /// Shown in place of the upload while Google Photos is off: once, quietly.
    public static let moreDestinationsHint = "Uploads (Google Photos) can be turned on in Settings ▸ Plugins."

    static func googlePhotosOffProblem(_ recipe: String) -> String {
        "“\(recipe)” uploads to Google Photos, which is turned off: turn it on in Settings ▸ Plugins, or turn the upload off."
    }

    /// Whether the Export sheet offers the Google Photos upload for `recipe`: when the
    /// integration is on, or the recipe already uploads (so it can be turned off).
    public func offersGooglePhotos(_ recipe: ExportRecipe) -> Bool {
        googlePhotosOn() || recipe.googlePhotos != nil
    }

    /// List this app's albums for the album picker (once signed in).
    public func loadGoogleAlbums() async {
        guard googlePhotosOn(), let googlePhotos, googlePhotos.isSignedIn else { return }
        do {
            googleAlbums = try await googlePhotos.albums().sorted {
                $0.title.localizedStandardCompare($1.title) == .orderedAscending
            }
            googleAlbumsMessage = nil
        } catch is CancellationError {
        } catch {
            googleAlbumsMessage = (error as? LocalizedError)?.errorDescription ?? "\(error)"
        }
    }

    /// Per recipe with a Google Photos destination, the files it wrote, each described
    /// by its photo's caption (else its title).
    static func googlePhotosJobs(_ outcome: ExportOutcome, job: ExportJob)
        -> [(recipe: ExportRecipe, target: GooglePhotosTarget, files: [GooglePhotosFile])] {
        let byPhoto = Dictionary(job.items.map { ($0.photoID, $0) }, uniquingKeysWith: { a, _ in a })
        return job.recipes.compactMap { r in
            guard let target = r.googlePhotos else { return nil }
            let files = outcome.written.filter { $0.recipeName == r.name }.map { w in
                GooglePhotosFile(url: w.url, description: byPhoto[w.photoID].flatMap { description(of: $0.metadata) })
            }
            return files.isEmpty ? nil : (r, target, files)
        }
    }

    /// The photo's caption, else its title (nil when both are empty).
    static func description(of m: IPTCMetadata) -> String? {
        [m.caption, m.title].map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.first { !$0.isEmpty }
    }

    /// Upload what the run wrote for recipes with a Google Photos destination, one
    /// recipe after another (Google asks for one batch at a time per user).
    func uploadToGooglePhotos(_ outcome: ExportOutcome, job: ExportJob) async {
        googleUploads = []
        guard let googlePhotos, !outcome.cancelled else { return }
        defer { uploading = nil }
        for (recipe, target, files) in Self.googlePhotosJobs(outcome, job: job) {
            if Task.isCancelled { break }
            // Turned off since the export started: the files stay exported, nothing is sent.
            guard googlePhotosOn() else {
                googleUploads.append(GooglePhotosUpload(recipeName: recipe.name, albumTitle: target.albumTitle,
                                                        error: "turned off in Settings ▸ Plugins; nothing was uploaded"))
                continue
            }
            uploading = GooglePhotosProgress(done: 0, total: files.count)
            let (updates, sink) = AsyncStream.makeStream(of: GooglePhotosProgress.self, bufferingPolicy: .bufferingNewest(1))
            let pump = Task { [weak self] in for await u in updates { self?.uploading = u } }
            var result: GooglePhotosUpload
            do {
                result = try await googlePhotos.upload(files, to: target) { sink.yield($0) }
            } catch is CancellationError {
                result = GooglePhotosUpload(cancelled: true)
            } catch {
                result = GooglePhotosUpload(error: (error as? LocalizedError)?.errorDescription ?? "\(error)")
            }
            sink.finish()
            await pump.value // the last update lands before `uploading` is cleared
            result.recipeName = recipe.name
            if result.albumTitle == nil { result.albumTitle = target.albumTitle }
            googleUploads.append(result)
        }
    }
}

/// Resumes a continuation once.
private final class ExportOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var taken = false
    func claim() -> Bool { lock.withLock { defer { taken = true }; return !taken } }
}
