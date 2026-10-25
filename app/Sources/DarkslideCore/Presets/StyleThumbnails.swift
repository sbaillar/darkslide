import CoreGraphics
import Foundation
import Observation

/// Thumbnails of each style applied to the focused photo (the Styles panel's tiles).
///
/// Each tile is the focused photo's current edit as a click on the style leaves it (a
/// look's tile applies the look, from the values before it), rendered by
/// `render` (live: the develop pipeline's disk-cached thumbnail, background priority).
/// Tiles render one after another, most useful first; a new photo or a new edit
/// cancels the pass and starts over. After a new edit (a click on a style) the previous
/// tiles stay in `stale`, shown dimmed until each new one lands, so the panel never
/// blanks. The Original tile (`originalID`: the photo with no adjustments) doesn't depend
/// on the edit and renders once per photo.
@MainActor @Observable
public final class StyleThumbnails {
    public typealias Render = @Sendable (Photo, EditState) async throws -> CGImage

    public private(set) var images: [UUID: CGImage] = [:]
    /// The previous edit's tiles, until their replacement renders.
    public private(set) var stale: [UUID: CGImage] = [:]
    /// The Original tile's key in `images`.
    public nonisolated static let originalID = UUID(uuidString: "6F726967-696E-616C-0000-000000000000")!
    /// Renders started (for tests and diagnostics).
    public private(set) var renders = 0
    @ObservationIgnored private let render: Render?
    @ObservationIgnored private var photo: PhotoID?
    @ObservationIgnored private var photoURL: URL?
    @ObservationIgnored private var base: EditState?
    @ObservationIgnored private var task: Task<Void, Never>?

    public init(render: Render?) {
        self.render = render
    }

    /// The live renderer: the develop pipeline's edited thumbnail (nil without develop tools).
    public nonisolated static func render(with tools: (any DevelopTools)?) -> Render? {
        guard let tools else { return nil }
        return { photo, edit in try await tools.editedThumbnail(for: photo, edit: edit).image }
    }

    /// Whether tiles can show at all (live mode with the develop pipeline).
    public var isAvailable: Bool { render != nil }

    /// Make the tiles show `presets` applied to `photo`'s `edit`.
    public func refresh(photo: Photo?, edit: EditState, presets: [Preset]) {
        guard let render else { return }
        guard let photo, photo.url != nil, !photo.isVideo else {
            task?.cancel()
            images = [:]
            stale = [:]
            self.photo = nil
            return
        }
        // Ids repeat across sessions: the file tells a new photo from the same id elsewhere.
        let newPhoto = self.photo != photo.id || photoURL != photo.url
        if newPhoto || base != edit {
            task?.cancel()
            let original = newPhoto ? nil : images[Self.originalID]
            stale = newPhoto ? [:] : stale.merging(images) { _, newer in newer }
            stale[Self.originalID] = nil // kept current, below
            images = [:]
            images[Self.originalID] = original
            self.photo = photo.id
            photoURL = photo.url
            base = edit
        }
        var missing: [(UUID, EditState)] = images[Self.originalID] == nil ? [(Self.originalID, EditState())] : []
        missing += presets.filter { images[$0.id] == nil }.map { ($0.id, LookPack.applying($0, to: edit)) }
        guard !missing.isEmpty else { return }
        task?.cancel()
        task = Task { [weak self] in
            for (id, tileEdit) in missing {
                if Task.isCancelled { return }
                guard let self else { return }
                self.renders += 1
                guard let image = try? await render(photo, tileEdit) else { continue }
                guard !Task.isCancelled, self.photo == photo.id, self.base == edit else { return }
                self.images[id] = image
                self.stale[id] = nil
            }
        }
    }

    /// A preset changed (update/rename): its tile renders again on the next refresh.
    public func invalidate(_ id: UUID) {
        images[id] = nil
        stale[id] = nil
    }

    /// The tile to show for `id`: the current one, else the previous edit's (`isStale`).
    public func tile(_ id: UUID) -> (image: CGImage, isStale: Bool)? {
        if let i = images[id] { return (i, false) }
        return stale[id].map { ($0, true) }
    }

    /// Wait for the current pass (tests).
    public func settle() async {
        await task?.value
    }

    public func cancel() {
        task?.cancel()
    }
}
