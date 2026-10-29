import CoreGraphics
import Foundation
import LuxtableAI

// Photo ▸ Merge to HDR… (⇧⌘H), Merge to Panorama… (⇧⌘M) and Merge to Best Shot… (⇧⌘B):
// two or more photos of the open session become one new photo next to them (lt-merge
// through the FFI `merge_preview` / `merge_photos`, `burst_preview` / `merge_burst`). The
// sources are never changed. A Best Shot takes, from the other frames of a burst, each
// face that looks better (open eyes first) and the regions that are sharper; it becomes
// the burst's cover unless the user chose one (`StackCover.bestShot`).

public enum PhotoMergeKind: String, Sendable, CaseIterable {
    case hdr, panorama, burst

    public var title: String {
        switch self {
        case .hdr: "Merge to HDR"
        case .panorama: "Merge to Panorama"
        case .burst: "Merge to Best Shot"
        }
    }
}

/// Best Shot: what Vision found on one source, and how good the frame is overall.
public struct BestShotFrame: Equatable, Sendable {
    public struct Face: Equatable, Sendable {
        /// Normalized to the upright photo, top-left origin (Vision's flipped).
        public var bounds: CGRect
        /// 0…1 that both eyes are open (`EyeMetrics.openProbability`); nil: unknown.
        public var eyesOpen: Double?
        /// Vision's capture quality 0…1; nil: unknown.
        public var quality: Double?

        public init(bounds: CGRect, eyesOpen: Double?, quality: Double?) {
            self.bounds = bounds
            self.eyesOpen = eyesOpen
            self.quality = quality
        }
    }

    public var faces: [Face]
    /// The last AI Cull's score 0…1, if it rated this frame.
    public var score: Double?

    public init(faces: [Face] = [], score: Double? = nil) {
        self.faces = faces
        self.score = score
    }
}

extension BestShotFrame.Face {
    /// A face Vision found (bottom-left origin) on the upright preview.
    init(_ f: FaceInfo) {
        let r = f.boundingBox
        self.init(bounds: CGRect(x: r.minX, y: 1 - r.maxY, width: r.width, height: r.height),
                  eyesOpen: EyeMetrics.openProbability(left: f.leftEyeAspect, right: f.rightEyeAspect),
                  quality: f.captureQuality)
    }
}

/// What the merge sheet lets the user choose. HDR uses `align` and `deghost`; a
/// panorama uses `projection` and `autoCrop`; a Best Shot uses `bestShot` (found by the
/// sheet before its first preview, not chosen).
public struct PhotoMergeOptions: Equatable, Sendable {
    public enum Deghost: String, Sendable, CaseIterable {
        case off, low, medium, high

        public var title: String { rawValue.capitalized }
    }

    public enum Projection: String, Sendable, CaseIterable {
        case auto, spherical, cylindrical, perspective

        public var title: String { rawValue.capitalized }
    }

    public var kind: PhotoMergeKind
    public var align = true
    /// Off by default, as in Lightroom.
    public var deghost = Deghost.off
    public var projection = Projection.auto
    public var autoCrop = true
    /// Best Shot: one per source, in source order (none: no faces, no scores).
    public var bestShot: [BestShotFrame] = []

    public init(kind: PhotoMergeKind) {
        self.kind = kind
    }
}

/// The merged result developed with its initial edit, for the sheet.
public struct PhotoMergePreview: @unchecked Sendable {
    public var image: CGImage
    /// The merged photo's full size.
    public var width: Int
    public var height: Int
    /// The projection used (Auto resolved; Perspective for HDR).
    public var projection: PhotoMergeOptions.Projection
    /// HDR: regions where one exposure was used (0 for panoramas).
    public var ghostRegions: Int
    /// The initial exposure the new photo gets.
    public var exposureEV: Double
    /// HDR: frames that could not be aligned (merged as they are). Best Shot: frames that
    /// could not be aligned (nothing was taken from them).
    public var unalignedFrames: Int
    /// Best Shot: faces taken from another frame (usually open eyes).
    public var facesReplaced: Int
    /// Best Shot: regions taken from a sharper frame.
    public var regionsReplaced: Int
    /// Best Shot: better faces left as they were (the head moved too much to blend).
    public var facesKept: Int

    public init(image: CGImage, width: Int, height: Int, projection: PhotoMergeOptions.Projection,
                ghostRegions: Int = 0, exposureEV: Double = 0, unalignedFrames: Int = 0,
                facesReplaced: Int = 0, regionsReplaced: Int = 0, facesKept: Int = 0) {
        self.image = image
        self.width = width
        self.height = height
        self.projection = projection
        self.ghostRegions = ghostRegions
        self.exposureEV = exposureEV
        self.unalignedFrames = unalignedFrames
        self.facesReplaced = facesReplaced
        self.regionsReplaced = regionsReplaced
        self.facesKept = facesKept
    }
}

/// Merges photos into a new one (live: lt-ffi; demo: `MockPhotoMerger`).
public protocol PhotoMerging: Sendable {
    /// A preview of the merge, its long edge at most `maxEdge` pixels. Cancelling the
    /// calling task cancels the work.
    func preview(_ photos: [Photo], options: PhotoMergeOptions, maxEdge: Int) async throws -> PhotoMergePreview
    /// Merge `photos` of `session` into a new photo registered in the session; returns its id.
    /// `progress` (0…1) is called off the main actor. Cancelling the calling task cancels it.
    func merge(_ photos: [PhotoID], in session: SessionInfo, options: PhotoMergeOptions,
               progress: @escaping @Sendable (Double) -> Void) async throws -> PhotoID
}

/// The merge sheet's state: options, a debounced preview that follows them, and the
/// merge itself (progress, cancel, error).
@MainActor @Observable
public final class MergeModel {
    public let session: SessionInfo
    public let sources: [Photo]
    /// Changing them previews again (debounced; the running preview is cancelled).
    public var options: PhotoMergeOptions {
        didSet { if options != oldValue { refreshPreview() } }
    }

    public private(set) var preview: PhotoMergePreview?
    public private(set) var isPreviewing = false
    /// 0…1 while merging, else nil.
    public private(set) var progress: Double?
    /// Why the last preview failed: cleared by the next preview. It doesn't block Merge:
    /// the small preview can fail to stitch what the full-size merge stitches.
    public private(set) var previewError: String?
    /// Why the last merge failed.
    public private(set) var mergeError: String?
    /// Best Shot: Vision is looking at the sources (before the first preview).
    public private(set) var isAnalyzing = false

    @ObservationIgnored let merger: any PhotoMerging
    @ObservationIgnored let debounce: Duration
    @ObservationIgnored let maxEdge: Int
    /// After a merge: list the session again and select the new photo.
    @ObservationIgnored let onMerged: @MainActor (PhotoID) async -> Void
    /// Best Shot: the faces and scores of the sources, one per source (nil: none).
    @ObservationIgnored let analyze: (@MainActor ([Photo]) async -> [BestShotFrame])?
    /// Close the sheet.
    @ObservationIgnored let close: @MainActor () -> Void
    @ObservationIgnored private(set) var previewTask: Task<Void, Never>?
    @ObservationIgnored private(set) var mergeTask: Task<Void, Never>?
    @ObservationIgnored private(set) var analysisTask: Task<Void, Never>?

    init(kind: PhotoMergeKind, session: SessionInfo, sources: [Photo], merger: any PhotoMerging,
         debounce: Duration = .milliseconds(300), maxEdge: Int = 1200,
         analyze: (@MainActor ([Photo]) async -> [BestShotFrame])? = nil,
         onMerged: @escaping @MainActor (PhotoID) async -> Void, close: @escaping @MainActor () -> Void) {
        self.options = PhotoMergeOptions(kind: kind)
        self.session = session
        self.sources = sources
        self.merger = merger
        self.debounce = debounce
        self.maxEdge = maxEdge
        self.analyze = analyze
        self.onMerged = onMerged
        self.close = close
    }

    public var kind: PhotoMergeKind { options.kind }
    public var title: String { "\(kind.title) (\(sources.count) Photos)" }
    public var isMerging: Bool { progress != nil }
    /// The message shown in the sheet.
    public var error: String? { mergeError ?? previewError }
    /// A Best Shot waits for Vision: merging without it would miss the faces.
    public var canMerge: Bool { !isMerging && !isAnalyzing && sources.count >= 2 }

    /// The projection menu's Auto item: "Auto (Cylindrical)" once the preview chose one.
    public var autoProjectionTitle: String {
        guard let p = preview?.projection, p != .auto else { return "Auto" }
        return "Auto (\(p.title))"
    }

    /// One line under the preview: size, exposure, (HDR) ghost regions and (Best Shot)
    /// what was taken from the other frames.
    public var previewSummary: String? {
        guard let p = preview else { return isAnalyzing ? "Looking for faces\u{2026}" : nil }
        var parts = ["\(p.width) × \(p.height)"]
        if p.exposureEV != 0 { parts.append(String(format: "exposure %+.2f EV", p.exposureEV)) }
        if kind == .hdr, p.ghostRegions > 0 {
            parts.append("\(p.ghostRegions) deghosted region\(p.ghostRegions == 1 ? "" : "s")")
        }
        if kind == .hdr, p.unalignedFrames > 0 {
            parts.append("\(p.unalignedFrames) frame\(p.unalignedFrames == 1 ? "" : "s") not aligned")
        }
        if kind == .burst { parts += Self.bestShotParts(p) }
        return parts.joined(separator: " · ")
    }

    /// "1 face improved", "2 sharper regions", … ("nothing to improve" when none).
    static func bestShotParts(_ p: PhotoMergePreview) -> [String] {
        func n(_ count: Int, _ one: String, _ many: String) -> String { "\(count) \(count == 1 ? one : many)" }
        var parts: [String] = []
        if p.facesReplaced > 0 { parts.append(n(p.facesReplaced, "face improved", "faces improved")) }
        if p.regionsReplaced > 0 { parts.append(n(p.regionsReplaced, "sharper region", "sharper regions")) }
        if p.facesKept > 0 { parts.append(n(p.facesKept, "face moved too much to swap", "faces moved too much to swap")) }
        if p.unalignedFrames > 0 { parts.append(n(p.unalignedFrames, "frame not used", "frames not used")) }
        return parts.isEmpty ? ["the best frame as it is: nothing to improve"] : parts
    }

    /// The first preview, at once (a Best Shot first finds the faces in its sources).
    public func start() {
        guard kind == .burst, let analyze else { return refreshPreview(debounced: false) }
        isAnalyzing = true
        isPreviewing = true
        analysisTask = Task { [weak self, sources] in
            let frames = await analyze(sources)
            guard let self, !Task.isCancelled else { return }
            self.isAnalyzing = false
            self.options.bestShot = frames.count == sources.count ? frames : []
            self.refreshPreview(debounced: false) // replaces the debounced one `options` started
        }
    }

    /// Preview again with the current options, after `debounce` (cancelling the previous one).
    public func refreshPreview(debounced: Bool = true) {
        previewTask?.cancel()
        guard !isMerging else { return }
        isPreviewing = true
        let options = options
        let wait = debounced ? debounce : .zero
        previewTask = Task { [weak self, merger, sources, maxEdge] in
            if wait > .zero {
                do { try await Task.sleep(for: wait) } catch { return }
            }
            do {
                let p = try await merger.preview(sources, options: options, maxEdge: maxEdge)
                guard !Task.isCancelled else { return }
                self?.preview = p
                self?.previewError = nil
            } catch {
                guard !Task.isCancelled, !(error is CancellationError) else { return }
                self?.previewError = error.localizedDescription
            }
            self?.isPreviewing = false
        }
    }

    /// Merge with the current options; on success the sheet closes and the new photo is selected.
    public func merge() {
        guard canMerge else { return }
        let interrupted = isPreviewing // previewed again if the merge doesn't happen
        previewTask?.cancel()
        isPreviewing = false
        mergeError = nil
        progress = 0
        let options = options
        let ids = sources.map(\.id)
        let report: @Sendable (Double) -> Void = { [weak self] f in
            Task { @MainActor in self?.noteProgress(f) }
        }
        mergeTask = Task { [weak self, merger, session] in
            do {
                let id = try await merger.merge(ids, in: session, options: options, progress: report)
                await self?.onMerged(id) // still "merging" meanwhile: Merge stays off
                self?.progress = nil
                self?.close()
            } catch {
                self?.progress = nil
                if !(error is CancellationError) { self?.mergeError = error.localizedDescription }
                if interrupted { self?.refreshPreview(debounced: false) }
            }
        }
    }

    /// Progress only grows, and none lands after the merge ended.
    private func noteProgress(_ f: Double) {
        guard let p = progress else { return }
        progress = max(p, min(1, f))
    }

    /// The Cancel button: stops a running merge (the sheet stays), else closes the sheet.
    public func cancel() {
        if isMerging {
            mergeTask?.cancel()
        } else {
            analysisTask?.cancel()
            previewTask?.cancel()
            close()
        }
    }
}

extension MergeModel: Equatable {
    public nonisolated static func == (a: MergeModel, b: MergeModel) -> Bool { a === b }
}

extension AppModel {
    /// The selected photos a merge would use: RAW files only (JPEG, HEIC, TIFF and
    /// videos are left out; merges read the sensor data).
    var mergeSources: [Photo] {
        selectedIDs.compactMap { photo($0) }.filter {
            RustRaw.extensions.contains(URL(fileURLWithPath: $0.filename).pathExtension.lowercased())
        }
    }

    /// Photo ▸ Merge to HDR… / Merge to Panorama… are available: two or more photos selected.
    public var canMergePhotos: Bool {
        merger != nil && currentSession != nil && sheet == nil && mergeSources.count >= 2
    }

    /// The frames a Best Shot merges: the selected RAWs; with one photo selected, the RAWs
    /// of its burst (a collapsed stack shows one frame). Earlier Best Shots are left out.
    var bestShotSources: [Photo] {
        let shots = Set(browserState.covers.filter(\.bestShot).map(\.cover))
        let raw = { (p: Photo) in
            !shots.contains(p.id)
                && RustRaw.extensions.contains(URL(fileURLWithPath: p.filename).pathExtension.lowercased())
        }
        let selected = selectedIDs.compactMap { photo($0) }.filter(raw)
        guard selectedIDs.count == 1, let id = selectedIDs.first, let s = browserState.stacks.stack(of: id) else {
            return selected
        }
        return s.memberIDs.compactMap { photo($0) }.filter(raw)
    }

    /// Photo ▸ Merge to Best Shot… is available: two or more frames (see `bestShotSources`).
    public var canMergeBestShot: Bool {
        merger != nil && currentSession != nil && sheet == nil && bestShotSources.count >= 2
    }

    /// Show the merge sheet for the selected photos and start its preview.
    public func beginMerge(_ kind: PhotoMergeKind) {
        guard kind == .burst ? canMergeBestShot : canMergePhotos, let merger, let session = currentSession else { return }
        let sources = kind == .burst ? bestShotSources : mergeSources
        let ids = sources.map(\.id)
        var analyze: (@MainActor ([Photo]) async -> [BestShotFrame])?
        if kind == .burst { analyze = { [weak self] photos in await self?.bestShotFrames(photos) ?? [] } }
        let m = MergeModel(kind: kind, session: session, sources: sources, merger: merger, analyze: analyze,
                           onMerged: { [weak self] id in
                               if kind == .burst { self?.bestShotMade(id, from: ids) }
                               await self?.mergeFinished(id)
                           },
                           close: { [weak self] in self?.closeMergeSheet() })
        sheet = .merge(m)
        m.start()
    }

    /// Vision's faces on each source's preview, and the last AI Cull's scores.
    func bestShotFrames(_ photos: [Photo]) async -> [BestShotFrame] {
        var frames: [BestShotFrame] = []
        for p in photos {
            var faces: [BestShotFrame.Face] = []
            if let image = await analysisImage(for: p), let found = try? await VisionService().faces(image) {
                faces = found.map { BestShotFrame.Face($0) }
            }
            frames.append(BestShotFrame(faces: faces, score: cullSuggestions[p.id]?.score))
        }
        return frames
    }

    /// A Best Shot of `sources` was made: it is their burst's cover unless the user chose one.
    func bestShotMade(_ id: PhotoID, from sources: [PhotoID]) {
        let b = browserState
        let covers = b.addBestShot(id, sources: sources)
        guard let session = currentSession else { return }
        let previous = b.coverSave
        let store = sessions
        b.coverSave = Task { [weak self] in
            await previous?.value // saves land in the order they were made
            do {
                try await store.saveStackCovers(covers, in: session)
            } catch {
                self?.show(error)
            }
        }
    }

    /// The new photo is in the session: list it again and select the new photo.
    func mergeFinished(_ id: PhotoID) async {
        await saver.flush()
        await refreshCurrentSession()
        if photo(id) != nil { focus(id, extend: false) }
    }

    func closeMergeSheet() {
        if case .merge = sheet { sheet = nil }
    }
}
