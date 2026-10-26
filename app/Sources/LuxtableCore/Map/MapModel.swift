import CoreGraphics
import CoreTransferable
import LuxtableFFI
import Foundation
import Observation
import UniformTypeIdentifiers

/// A located photo of some session, from the global index.
public struct RemoteMapPoint: Hashable, Sendable {
    public var sessionRoot: URL
    public var sessionName: String
    public var url: URL
    public var location: GeoPoint
    /// Best rating of its variants and the master's flag (the browser's filter applies).
    public var rating: Int
    public var pick: PickState

    public init(sessionRoot: URL, sessionName: String, url: URL, location: GeoPoint, rating: Int = 0,
                pick: PickState = .none) {
        self.sessionRoot = sessionRoot
        self.sessionName = sessionName
        self.url = url
        self.location = location
        self.rating = rating
        self.pick = pick
    }
}

/// Where "All Sessions" gets every session's located photos (seam for tests).
public protocol MapPointSource: Sendable {
    func points() async throws -> [RemoteMapPoint]
}

/// `MapPointSource` over the global index through the FFI.
public struct RustMapPointSource: MapPointSource {
    public let globalIndex: URL

    public init(globalIndex: URL = RustImportService.defaultGlobalIndex) {
        self.globalIndex = globalIndex
    }

    public func points() async throws -> [RemoteMapPoint] {
        let g = globalIndex.path
        let records = try await RustSessionStore.call { token in try await globalMapPoints(globalIndex: g, token: token) }
        return records.compactMap(Self.point)
    }

    /// A record as a point (`nil` when its position is out of range).
    static func point(_ r: MapPointRecord) -> RemoteMapPoint? {
        GeoPoint(latitude: r.latitude, longitude: r.longitude).map {
            RemoteMapPoint(sessionRoot: URL(fileURLWithPath: r.sessionRoot), sessionName: r.sessionName,
                           url: URL(fileURLWithPath: r.path), location: $0, rating: Int(r.rating),
                           pick: RustSessionStore.pick(r.pick))
        }
    }
}

/// What the map view knows about itself (seam for tests): its size and the
/// position under a point, from MapKit in the app and a plain projection in tests.
@MainActor
public protocol MapSurface {
    var size: CGSize { get }
    func location(at point: CGPoint) -> GeoPoint?
}

extension UTType {
    /// Photos dragged from the filmstrip (declared in Info.plist).
    public static let luxtablePhotos = UTType(exportedAs: "app.luxtable.Luxtable.photos")
}

/// Photos dragged from the filmstrip onto the map: their variant ids, which only
/// mean something in the session and the app instance they came from.
public struct PhotoDragItem: Hashable, Sendable, Codable, Transferable {
    /// This running app (another instance's ids are other photos).
    public static let thisInstance = UUID()

    public var ids: [PhotoID]
    public var session: UUID
    public var instance: UUID

    public init(ids: [PhotoID], session: UUID, instance: UUID = PhotoDragItem.thisInstance) {
        self.ids = ids
        self.session = session
        self.instance = instance
    }

    public static var transferRepresentation: some TransferRepresentation {
        CodableRepresentation(contentType: .luxtablePhotos)
    }
}

/// [map] The Map mode (like Lightroom's Map module): where the session's photos
/// were taken, as clustered markers; a click filters the browser to a marker's
/// photos, the visible or a drawn area can filter it, and photos dropped on
/// the map are geotagged (sidecar only; the original is never written).
///
/// Markers are clustered off the main actor, only around what's on screen and
/// at most `MapClustering.defaultLimit` of them; `refresh()` runs it when the
/// camera, the photos, the filter or the scope change (never for a selection).
@MainActor @Observable
public final class MapModel {
    public enum Scope: String, CaseIterable, Sendable {
        case session = "This Session"
        case allSessions = "All Sessions"
    }

    public enum Loading: Equatable, Sendable {
        case idle, loading
        case failed(String)
    }

    /// Whether the map may load tiles from Apple Maps (asked once; Settings can change it).
    public enum Tiles: Equatable, Sendable { case ask, on, off }

    public static let tilesNotice =
        "Map tiles come from Apple Maps. The areas you view are requested from Apple; your photos and their locations aren't uploaded."

    public var scope: Scope = .session {
        didSet { if scope != oldValue { Task { await reloadRemote() } } }
    }
    /// What the map shows now (reported by the view as the camera moves; kept
    /// while the view is away, so coming back to Map mode keeps the camera).
    public private(set) var region: GeoRegion?
    public private(set) var viewSize: CGSize = .zero
    /// Keep the browser filtered to the photos in view as the map moves.
    public var filterToView = false {
        didSet {
            guard filterToView != oldValue, let app else { return }
            if filterToView, let region { app.filter.place = .area(region) }
            if !filterToView, case .area = app.filter.place { app.filter.place = nil }
        }
    }
    /// Dragging on the map draws an area instead of panning.
    public var drawingArea = false
    /// Photos of other sessions (All Sessions), the open session's left out.
    public private(set) var remote: [RemoteMapPoint] = []
    public private(set) var loading: Loading = .idle
    /// The other sessions' photos of the last marker clicked (the view offers to open them).
    public private(set) var remoteChoice: [MapPin.Remote] = []
    /// Bumped to ask the view to move its camera to `requestedRegion`.
    public private(set) var cameraRequest = 0
    public private(set) var requestedRegion: GeoRegion?
    /// The markers for the current camera (see the type's docs).
    public private(set) var clusters: [MapCluster] = []
    /// Located photos in the open session (for "n of m located").
    public private(set) var locatedCount = 0

    @ObservationIgnored public var source: (any MapPointSource)?
    @ObservationIgnored weak var app: AppModel?
    @ObservationIgnored private var loadTicket = 0
    @ObservationIgnored private var clusterTicket = 0
    @ObservationIgnored private var clusterTask: Task<Void, Never>?
    /// The pins of the last refresh and what they were built from.
    @ObservationIgnored private var cachedPins: [MapPin] = []
    @ObservationIgnored private var pinsBuiltFrom: PinInputs?

    private struct PinInputs: Equatable {
        var photosVersion: Int
        var filter: PhotoFilter
        var remoteVersion: Int
        var scope: Scope
    }

    @ObservationIgnored private var remoteVersion = 0

    public init() {}

    func attach(_ app: AppModel) { self.app = app }

    public func configure(source: any MapPointSource) { self.source = source }

    // MARK: - Tiles

    public var tiles: Tiles {
        switch app?.settings.mapTiles {
        case true?: .on
        case false?: .off
        case nil: .ask
        }
    }

    /// The first-use notice was answered (Show Map / Not Now), or Settings changed it.
    public func allowTiles(_ on: Bool) { app?.settings.mapTiles = on }

    // MARK: - Pins and clusters

    /// The open session's located photos under the browser's filter (its map
    /// part aside, so a click never empties the map), plus other sessions'
    /// under its rating and flag criteria. Rebuilt only when its inputs change.
    public var pins: [MapPin] {
        guard let app else { return [] }
        var f = app.filter
        f.place = nil
        let inputs = PinInputs(photosVersion: app.photosVersion, filter: f, remoteVersion: remoteVersion, scope: scope)
        if inputs == pinsBuiltFrom { return cachedPins }
        var located = 0
        var local: [MapPin] = []
        for p in app.photos {
            guard let loc = p.location else { continue }
            if !p.inTrash { located += 1 }
            if f.matches(p) { local.append(MapPin(photo: p.id, location: loc)) }
        }
        let others = scope == .allSessions
            ? remote.enumerated().compactMap { i, r -> MapPin? in
                Self.remoteMatches(r, f) ? MapPin(remote: .init(sessionRoot: r.sessionRoot, sessionName: r.sessionName,
                                                                url: r.url), location: r.location, index: i) : nil
            }
            : []
        cachedPins = local + others
        pinsBuiltFrom = inputs
        if locatedCount != located { locatedCount = located }
        return cachedPins
    }

    /// The browser's rating and flag criteria, for another session's photo.
    nonisolated static func remoteMatches(_ r: RemoteMapPoint, _ f: PhotoFilter) -> Bool {
        if r.rating < f.minRating || (f.unratedOnly && r.rating > 0) { return false }
        switch f.pick {
        case .all: return true
        case .picked: return r.pick == .picked
        case .unflagged: return r.pick == .none
        case .unrejected: return r.pick != .rejected
        case .rejected: return r.pick == .rejected
        }
    }

    /// Cluster again (off the main actor) for the current camera, photos, filter and scope.
    public func refresh() {
        let pins = pins
        let region = region ?? GeoRegion(south: -85, west: -180, north: 85, east: 180)
        let size = viewSize.width > 0 ? viewSize : CGSize(width: 1000, height: 600)
        clusterTicket += 1
        let mine = clusterTicket
        clusterTask?.cancel()
        clusterTask = Task { [weak self] in
            let result = await Task.detached(priority: .userInitiated) {
                MapClustering.clusters(pins, region: region, size: size)
            }.value
            guard let self, !Task.isCancelled, mine == self.clusterTicket else { return }
            if self.clusters != result { self.clusters = result }
        }
    }

    /// `refresh()`, waiting for the markers (tests and callers that need them now).
    public func refreshed() async {
        refresh()
        await clusterTask?.value
    }

    /// A marker holds the focused or a selected photo (the browser's selection,
    /// mirrored; read by each marker, so a selection change never re-clusters).
    public func isHighlighted(_ c: MapCluster) -> Bool {
        guard let app else { return false }
        return c.photoIDs.contains { $0 == app.focusedID || app.selection.contains($0) }
    }

    // MARK: - Camera

    /// The view's camera moved (or the view resized).
    public func cameraChanged(_ region: GeoRegion, size: CGSize) {
        self.region = region
        viewSize = size
        if filterToView, let app, app.filter.place != .area(region) { app.filter.place = .area(region) }
        refresh()
    }

    /// The view appeared: back where it was, else every marker framed.
    public func appeared() {
        if let region { request(region) } else { showAll() }
        refresh()
    }

    /// Frame every marker (never closer than about a city, so opening the map
    /// doesn't zoom onto one house).
    public func showAll() {
        guard let r = GeoRegion.enclosing(pins.map(\.location)) else { return }
        request(r)
    }

    /// Frame the browser's selection (or the focused photo) when it has a position.
    public func showSelection() {
        guard let app else { return }
        let located = app.selectedIDs.compactMap { app.photo($0)?.location }
        guard let r = GeoRegion.enclosing(located) else { return }
        request(r)
    }

    /// Follow the browser: the newly focused photo comes into view if it's off screen.
    public func focusChanged() {
        guard let app, let p = app.focusedPhoto?.location, let region, !region.contains(p) else { return }
        request(GeoRegion(center: p, latitudeDelta: region.latitudeSpan, longitudeDelta: region.longitudeSpan))
    }

    private func request(_ r: GeoRegion) {
        requestedRegion = r
        cameraRequest += 1
    }

    // MARK: - Filtering

    /// A marker was clicked: the browser shows (and selects) its photos.
    /// Other sessions' photos are offered to open instead.
    public func select(_ c: MapCluster) {
        remoteChoice = c.remotePins
        guard let app else { return }
        let ids = c.photoIDs
        guard !ids.isEmpty else { return }
        filterToView = false
        app.filter.place = .photos(Set(ids))
        let shown = app.visibleIDs.filter(Set(ids).contains)
        if let first = shown.first {
            app.focus(first, extend: false)
            app.selection = Set(shown)
        }
    }

    /// A rectangle was drawn between two points of `surface`: west is its left
    /// edge and east its right one, so a box wider than half the world selects
    /// what it covers (the whole world when it spans more than all of it).
    public func drewArea(from a: CGPoint, to b: CGPoint, on surface: any MapSurface) {
        drawingArea = false
        let (l, r) = a.x <= b.x ? (a, b) : (b, a)
        guard let app, let left = surface.location(at: l), let right = surface.location(at: r) else { return }
        filterToView = false
        var area = GeoRegion(left: left, right: right)
        let width = surface.size.width
        if let region, width > 0, Double((r.x - l.x) / width) * region.longitudeSpan >= 360 {
            area = GeoRegion(south: area.south, west: -180, north: area.north, east: 180)
        }
        app.filter.place = .area(area)
    }

    /// Leave the map's filter (the rest of the browser's filter stays).
    public func clearFilter() {
        filterToView = false
        app?.filter.place = nil
    }

    /// The browser's map filter went away (Clear, a smart folder): stop following the view.
    func placeFilterCleared() {
        if filterToView { filterToView = false }
    }

    // MARK: - Geotagging

    /// The drag payload for a drag starting on `id`: the selection when `id`
    /// is in it, else just `id`; `nil` with no session open.
    public func dragItem(from id: PhotoID) -> PhotoDragItem? {
        guard let app, let session = app.currentSession else { return nil }
        let sel = app.selectedIDs
        return PhotoDragItem(ids: sel.contains(id) ? sel : [id], session: session.id)
    }

    /// Photos dropped at `point` take that position (one undo step). A drag from
    /// another app instance or another session is refused (its ids are other photos).
    @discardableResult
    public func drop(_ item: PhotoDragItem, at point: CGPoint, on surface: any MapSurface) -> Bool {
        guard let app, item.instance == PhotoDragItem.thisInstance, item.session == app.currentSession?.id else {
            app?.show(Toast("Drag photos from this window's filmstrip to set their location", symbol: "mappin.slash"))
            return false
        }
        guard let location = surface.location(at: point) else { return false }
        return setLocation(location, for: item.ids)
    }

    /// Geotag `ids` (and every other variant of the same originals: a position
    /// belongs to the photo) at `location`; returns whether any photo changed.
    @discardableResult
    public func setLocation(_ location: GeoPoint, for ids: [PhotoID]) -> Bool {
        guard let app else { return false }
        let targets = withSiblings(ids).filter { app.photo($0).map { !$0.isVideo && $0.geotag != location } ?? false }
        app.updatePhotos(targets, label: targets.count == 1 ? "Set Location" : "Set Location of \(targets.count) Photos") {
            $0.geotag = location
        }
        return !targets.isEmpty
    }

    /// Remove the geotag of `ids` (and their variants): the file's own GPS (if any) shows again.
    public func removeLocation(of ids: [PhotoID]) {
        guard let app else { return }
        let targets = withSiblings(ids).filter { app.photo($0)?.geotag != nil }
        app.updatePhotos(targets, label: "Remove Location") { $0.geotag = nil }
    }

    /// `ids` plus the other variants of their originals, in session order.
    private func withSiblings(_ ids: [PhotoID]) -> [PhotoID] {
        guard let app else { return ids }
        let urls = Set(ids.compactMap { app.photo($0)?.url })
        guard !urls.isEmpty else { return ids }
        let siblings = app.photos.filter { $0.url.map(urls.contains) ?? false }.map(\.id)
        let known = Set(siblings)
        return siblings + ids.filter { !known.contains($0) }
    }

    /// Selected photos with a geotag (what "Remove Location" applies to).
    public var removableSelection: [PhotoID] {
        guard let app else { return [] }
        return app.selectedIDs.filter { app.photo($0)?.geotag != nil }
    }

    // MARK: - All Sessions

    /// Reload other sessions' photos (All Sessions), after the open session's
    /// latest state reaches the global index.
    public func reloadRemote() async {
        loadTicket += 1
        let mine = loadTicket
        guard scope == .allSessions, let source else {
            setRemote([])
            loading = .idle
            return
        }
        loading = .loading
        if let app, let session = app.currentSession, let people = app.people.backend {
            // Best effort: an older copy of this session is still on the map.
            try? await people.syncFind(faceModel: app.people.modelID, suggest: app.people.tuning.autoMatch, in: session)
        }
        do {
            let all = try await source.points()
            let here = app?.currentSession?.url
            // Path normalizing touches no disk, but 50k of them stay off the main actor.
            let others = await Task.detached(priority: .userInitiated) {
                let mine = here?.standardizedFileURL.path
                return all.filter { $0.sessionRoot.standardizedFileURL.path != mine }
            }.value
            guard mine == loadTicket else { return }
            setRemote(others)
            loading = .idle
        } catch {
            guard mine == loadTicket else { return }
            setRemote([])
            loading = .failed(PeopleModel.message(error))
        }
    }

    private func setRemote(_ points: [RemoteMapPoint]) {
        guard points != remote else { return }
        remote = points
        remoteVersion += 1
        refresh()
    }

    /// Open the session of other-session photos and show them.
    public func open(_ pins: [MapPin.Remote]) async {
        guard let app, let root = pins.first?.sessionRoot else { return }
        remoteChoice = []
        await app.openFolder(root)
        let wanted = Set(pins.filter { $0.sessionRoot == root }.map { $0.url.standardizedFileURL.path })
        let ids = app.photos.filter { $0.url.map { wanted.contains($0.standardizedFileURL.path) } ?? false }.map(\.id)
        guard !ids.isEmpty else {
            if case .photos = app.filter.place { app.filter.place = nil } // never another session's ids
            return
        }
        app.filter.place = .photos(Set(ids))
        if let first = app.visibleIDs.first { app.focus(first, extend: false) }
        await reloadRemote()
    }

    /// A session among other-session photos, with how many there are.
    public struct RemoteSession: Hashable, Sendable {
        public var name: String
        public var root: URL
        public var count: Int
    }

    /// Sessions in a list of other-session photos, with their counts, by name.
    public static func sessions(_ pins: [MapPin.Remote]) -> [RemoteSession] {
        var counts: [URL: RemoteSession] = [:]
        for p in pins {
            counts[p.sessionRoot, default: RemoteSession(name: p.sessionName, root: p.sessionRoot, count: 0)].count += 1
        }
        return counts.values.sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    /// The open session changed: its own camera, filters and other sessions' photos start afresh.
    func sessionChanged() {
        remoteChoice = []
        filterToView = false
        drawingArea = false
        region = nil
        Task { await reloadRemote() }
        refresh()
    }
}
