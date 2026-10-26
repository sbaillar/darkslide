import CoreGraphics
import Foundation

// Photo ▸ Merge to HDR… (⇧⌘H) and Merge to Panorama… (⇧⌘M): two or more photos of the
// open session become one new photo next to them (lt-merge through the FFI
// `merge_preview` / `merge_photos`). The sources are never changed.

public enum PhotoMergeKind: String, Sendable, CaseIterable {
    case hdr, panorama

    public var title: String {
        switch self {
        case .hdr: "Merge to HDR"
        case .panorama: "Merge to Panorama"
        }
    }
}

/// What the merge sheet lets the user choose. HDR uses `align` and `deghost`; a
/// panorama uses `projection` and `autoCrop`.
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
    /// HDR: frames that could not be aligned (merged as they are).
    public var unalignedFrames: Int

    public init(image: CGImage, width: Int, height: Int, projection: PhotoMergeOptions.Projection,
                ghostRegions: Int = 0, exposureEV: Double = 0, unalignedFrames: Int = 0) {
        self.image = image
        self.width = width
        self.height = height
        self.projection = projection
        self.ghostRegions = ghostRegions
        self.exposureEV = exposureEV
        self.unalignedFrames = unalignedFrames
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

    @ObservationIgnored let merger: any PhotoMerging
    @ObservationIgnored let debounce: Duration
    @ObservationIgnored let maxEdge: Int
    /// After a merge: list the session again and select the new photo.
    @ObservationIgnored let onMerged: @MainActor (PhotoID) async -> Void
    /// Close the sheet.
    @ObservationIgnored let close: @MainActor () -> Void
    @ObservationIgnored private(set) var previewTask: Task<Void, Never>?
    @ObservationIgnored private(set) var mergeTask: Task<Void, Never>?

    init(kind: PhotoMergeKind, session: SessionInfo, sources: [Photo], merger: any PhotoMerging,
         debounce: Duration = .milliseconds(300), maxEdge: Int = 1200,
         onMerged: @escaping @MainActor (PhotoID) async -> Void, close: @escaping @MainActor () -> Void) {
        self.options = PhotoMergeOptions(kind: kind)
        self.session = session
        self.sources = sources
        self.merger = merger
        self.debounce = debounce
        self.maxEdge = maxEdge
        self.onMerged = onMerged
        self.close = close
    }

    public var kind: PhotoMergeKind { options.kind }
    public var title: String { "\(kind.title) (\(sources.count) Photos)" }
    public var isMerging: Bool { progress != nil }
    /// The message shown in the sheet.
    public var error: String? { mergeError ?? previewError }
    public var canMerge: Bool { !isMerging && sources.count >= 2 }

    /// The projection menu's Auto item: "Auto (Cylindrical)" once the preview chose one.
    public var autoProjectionTitle: String {
        guard let p = preview?.projection, p != .auto else { return "Auto" }
        return "Auto (\(p.title))"
    }

    /// One line under the preview: size, exposure and (HDR) ghost regions.
    public var previewSummary: String? {
        guard let p = preview else { return nil }
        var parts = ["\(p.width) × \(p.height)"]
        if p.exposureEV != 0 { parts.append(String(format: "exposure %+.2f EV", p.exposureEV)) }
        if kind == .hdr, p.ghostRegions > 0 {
            parts.append("\(p.ghostRegions) deghosted region\(p.ghostRegions == 1 ? "" : "s")")
        }
        if kind == .hdr, p.unalignedFrames > 0 {
            parts.append("\(p.unalignedFrames) frame\(p.unalignedFrames == 1 ? "" : "s") not aligned")
        }
        return parts.joined(separator: " · ")
    }

    /// The first preview, at once.
    public func start() { refreshPreview(debounced: false) }

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

    /// Show the merge sheet for the selected photos and start its preview.
    public func beginMerge(_ kind: PhotoMergeKind) {
        guard canMergePhotos, let merger, let session = currentSession else { return }
        let m = MergeModel(kind: kind, session: session, sources: mergeSources, merger: merger,
                           onMerged: { [weak self] id in await self?.mergeFinished(id) },
                           close: { [weak self] in self?.closeMergeSheet() })
        sheet = .merge(m)
        m.start()
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
