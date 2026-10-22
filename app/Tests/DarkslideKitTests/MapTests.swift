import CoreGraphics
import DarkslideFFI
import Foundation
import ImageIO
import SQLite3
import Testing
import UniformTypeIdentifiers
@testable import DarkslideKit

// [map] Geometry, clustering, the map filter, geotagging and All Sessions.

private func geo(_ lat: Double, _ lon: Double, _ alt: Double? = nil) -> GeoPoint {
    GeoPoint(latitude: lat, longitude: lon, altitude: alt)!
}

/// A plain Web Mercator surface (MapKit's projection stand-in): `region` fills `size`.
@MainActor
private struct FlatSurface: MapSurface {
    var region: GeoRegion
    var size: CGSize

    func location(at point: CGPoint) -> GeoPoint? {
        guard point.x >= 0, point.y >= 0, point.x <= size.width, point.y <= size.height else { return nil }
        let nw = WebMercator.project(geo(region.north, region.west))
        let se = WebMercator.project(geo(region.south, region.east))
        let spanX = se.x >= nw.x ? se.x - nw.x : se.x + 1 - nw.x // across the antimeridian
        return WebMercator.unproject(CGPoint(x: nw.x + spanX * point.x / size.width,
                                             y: nw.y + (se.y - nw.y) * point.y / size.height))
    }
}

/// A zoomed-out map whose left and right edges show the same longitude (10°).
@MainActor
private struct WrappedSurface: MapSurface {
    var size: CGSize
    func location(at point: CGPoint) -> GeoPoint? { geo(40 - 40 * Double(point.y / size.height), 10) }
}

/// Two sessions of the same eight photos: 1–2 on a Pacific island (EXIF), 3 in Sydney
/// below sea level, 4 and 5 with no position, 6 geotagged in Paris over EXIF in
/// Tokyo, 7 a video, 8 a second variant of 4. Saves are recorded.
final class GeoStore: SessionStore, @unchecked Sendable {
    static let session = SessionInfo(name: "Island", url: URL(fileURLWithPath: "/tmp/island"), photoCount: 8,
                                     createdAt: Date())
    static let other = SessionInfo(name: "Home", url: URL(fileURLWithPath: "/tmp/home"), photoCount: 8,
                                   createdAt: Date(timeIntervalSince1970: 0))
    private let lock = NSLock()
    private var _saved: [Photo] = []
    var saved: [Photo] { lock.withLock { _saved } }

    func listSessions() async throws -> [SessionInfo] { [Self.session, Self.other] }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        (1...8).map { i in
            var p = samplePhoto(Int64(i))
            p.url = session.url.appendingPathComponent("DSC0\(i == 8 ? 4 : i).ARW")
            p.meta.captureDate = Date(timeIntervalSince1970: 1_790_000_000 + Double(i))
            switch i {
            case 1: p.meta.gps = geo(22.2, -159.5)
            case 2: p.meta.gps = geo(22.2003, -159.5004); p.rating = 3
            case 3: p.meta.gps = geo(-33.86, 151.21, -4)
            case 6:
                p.meta.gps = geo(35.68, 139.69)
                p.geotag = geo(48.86, 2.35)
            case 7:
                p.filename = "C0007.MP4"
                p.url = session.url.appendingPathComponent("C0007.MP4")
            case 8: p.variantName = "Variant 2"
            default: break
            }
            return p
        }
    }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { Self.session }
    func save(photos: [Photo]) async throws { lock.withLock { _saved += photos } }
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
}

/// Other sessions' points, or a failure.
final class FakeMapSource: MapPointSource, @unchecked Sendable {
    let stored: [RemoteMapPoint]
    let error: Error?
    private(set) var calls = 0

    init(_ points: [RemoteMapPoint] = [], error: Error? = nil) {
        stored = points
        self.error = error
    }

    func points() async throws -> [RemoteMapPoint] {
        calls += 1
        if let error { throw error }
        return stored
    }
}

@MainActor
private func loaded(_ store: GeoStore = GeoStore()) async -> AppModel {
    let m = makeModel(store: store)
    await m.start()
    _ = await waitUntil { m.photoCount == 8 && m.currentSession == GeoStore.session }
    return m
}

private let world = GeoRegion(south: -85, west: -180, north: 85, east: 180)
private let island = GeoRegion(center: GeoPoint(latitude: 22.2, longitude: -159.5)!, latitudeDelta: 0.01,
                              longitudeDelta: 0.01)
private let remoteRoot = URL(fileURLWithPath: "/s")

private func remotePin(_ name: String = "a", index: Int = 0) -> MapPin.Remote {
    MapPin.Remote(sessionRoot: remoteRoot, sessionName: "S", url: remoteRoot.appendingPathComponent("\(name).ARW"))
}

// MARK: - Geometry

@Test func geoPointsAreValidated() {
    #expect(GeoPoint(latitude: 91, longitude: 0) == nil)
    #expect(GeoPoint(latitude: 0, longitude: -180.1) == nil)
    #expect(GeoPoint(latitude: .nan, longitude: 0) == nil)
    #expect(GeoPoint(latitude: 1, longitude: 2, altitude: .infinity)?.altitude == nil)
    #expect(GeoPoint(latitude: 0, longitude: 0) != nil, "a hand-dropped 0, 0 is a real place")
    #expect(geo(-33.5, -70.25).text == "33.5000° S, 70.2500° W")
    #expect(geo(22.2, 159.5).text == "22.2000° N, 159.5000° E")
}

@Test func regionsContainWrapAndExpand() {
    #expect(island.contains(geo(22.2, -159.5)))
    #expect(!island.contains(geo(22.3, -159.5)))
    #expect(!island.contains(geo(22.2, -159.6)))
    // Screen-left corner east of the right one: across the antimeridian (Fiji to Samoa).
    let pacific = GeoRegion(left: geo(-20, 175), right: geo(-10, -170))
    #expect(pacific.crossesAntimeridian)
    #expect(abs(pacific.longitudeSpan - 15) < 1e-9)
    #expect(pacific.contains(geo(-15, 179)) && pacific.contains(geo(-15, -175)))
    #expect(!pacific.contains(geo(-15, 0)))
    #expect(abs(pacific.center.longitude - -177.5) < 1e-9, "182.5° east, wrapped")
    // A box wider than half the world, left to right: what it covers, not the rest.
    let wide = GeoRegion(left: geo(10, -120), right: geo(40, 100))
    #expect(!wide.crossesAntimeridian && abs(wide.longitudeSpan - 220) < 1e-9)
    #expect(wide.contains(geo(30, 0)) && !wide.contains(geo(30, 170)))
    // The same corners the other way round: the Pacific side.
    #expect(GeoRegion(left: geo(10, 100), right: geo(40, -120)).contains(geo(30, 170)))
    // A whole-world span, clamping and wrapping.
    let all = GeoRegion(center: geo(0, 50), latitudeDelta: 400, longitudeDelta: 720)
    #expect(all.west == -180 && all.east == 180 && all.south == -90 && all.north == 90)
    #expect(GeoRegion.wrap(190) == -170 && GeoRegion.wrap(-190) == 170 && GeoRegion.wrap(.nan) == 0)
    #expect(GeoRegion(center: geo(0, 179), latitudeDelta: 1, longitudeDelta: 4).crossesAntimeridian)
    // One screen around: a small view grows threefold; a wide one covers the world.
    let near = island.expanded(by: 1)
    #expect(abs(near.longitudeSpan - 0.03) < 1e-9 && near.contains(geo(22.21, -159.51)))
    #expect(GeoRegion(center: geo(0, 0), latitudeDelta: 10, longitudeDelta: 150).expanded(by: 1).longitudeSpan == 360)
}

@Test func enclosingFramesNarrowlyAndNeverCloserThanACity() {
    #expect(GeoRegion.enclosing([]) == nil)
    // One house: about 20 km, not street level.
    let one = GeoRegion.enclosing([geo(22.2, -159.5)])!
    #expect(one.contains(geo(22.2, -159.5)))
    #expect(abs(one.latitudeSpan - 0.2) < 1e-9 && abs(one.longitudeSpan - 0.2) < 1e-9)
    let two = GeoRegion.enclosing([geo(0, 0), geo(10, 20)])!
    #expect(two.south < 0 && two.north > 10 && two.west < 0 && two.east > 20 && !two.crossesAntimeridian)
    // Fiji and Samoa: across the antimeridian, not the whole world.
    let pacific = GeoRegion.enclosing([geo(-18, 178), geo(-14, -172)])!
    #expect(pacific.crossesAntimeridian && pacific.longitudeSpan < 20)
    #expect(pacific.contains(geo(-16, 179)) && !pacific.contains(geo(-16, 0)))
    #expect(GeoRegion.enclosing([geo(0, -179.99), geo(0, 179.99)])!.longitudeSpan < 1)
    // Near the edge without crossing: clamped, not wrapped.
    let edge = GeoRegion.enclosing([geo(0, 179.95), geo(0, 170)])!
    #expect(!edge.crossesAntimeridian && edge.east == 180)
    // Spread everywhere: the whole world.
    #expect(GeoRegion.enclosing([-144, -72, 0, 72, 144].map { geo(0, $0) })!.longitudeSpan == 360)
    #expect(GeoRegion.enclosing([geo(0, -170), geo(0, -50), geo(0, 70)])!.longitudeSpan < 360, "the largest gap is left out")
}

@Test func webMercatorRoundTrips() {
    for p in [geo(0, 0), geo(22.2, -159.5), geo(-33.86, 151.21), geo(60, 179.9)] {
        let back = WebMercator.unproject(WebMercator.project(p))
        #expect(abs(back.latitude - p.latitude) < 1e-9 && abs(back.longitude - p.longitude) < 1e-9)
    }
    #expect(WebMercator.project(geo(0, 0)) == CGPoint(x: 0.5, y: 0.5))
    #expect(abs(WebMercator.project(geo(90, 0)).y) < 1e-9, "clamped at the Mercator limit")
    #expect(abs(WebMercator.unproject(CGPoint(x: 1.25, y: 0.5)).longitude - -90) < 1e-9, "wraps")
    #expect(abs(WebMercator.unproject(CGPoint(x: 0.5, y: -3)).latitude - WebMercator.maxLatitude) < 1e-6)
}

// MARK: - Clustering

@Test func clustersFollowTheZoomAndStayNearTheView() {
    let pins = [MapPin(photo: PhotoID(1), location: geo(22.2, -159.5)),
                MapPin(photo: PhotoID(2), location: geo(22.2003, -159.5004)),
                MapPin(photo: PhotoID(3), location: geo(-33.86, 151.21))]
    let size = CGSize(width: 1000, height: 600)
    #expect(MapClustering.clusters([], region: world, size: size).isEmpty)
    // The whole world: the island's two photos are one marker, Sydney another.
    let far = MapClustering.clusters(pins, region: world, size: size)
    #expect(far.map(\.count) == [2, 1])
    #expect(far[0].photoIDs == [PhotoID(1), PhotoID(2)])
    #expect(far[0].id == "p1" && far[0].key == 1 && far[0].remotePins.isEmpty)
    #expect(abs(far[0].location.latitude - 22.20015) < 1e-6, "the mean position")
    // Zoomed in to ~1 km: only the photos within a screen of the view, one marker each.
    let near = MapClustering.clusters(pins, region: island, size: size)
    #expect(near.map(\.id) == ["p1", "p2"], "Sydney is far off screen; stable order")
    // Remote pins cluster with local ones; ids carry the session root.
    let r = remotePin()
    let mixed = MapClustering.clusters(pins + [MapPin(remote: r, location: geo(22.2001, -159.5), index: 0)],
                                       region: world, size: size)
    #expect(mixed[0].count == 3 && mixed[0].remotePins == [r] && mixed[0].photoIDs.count == 2)
    #expect(mixed[0].key == -1, "remote keys sort first")
    let same = MapPin.Remote(sessionRoot: URL(fileURLWithPath: "/t"), sessionName: "T", url: r.url)
    #expect(MapPin(remote: r, location: geo(0, 0), index: 0).id != MapPin(remote: same, location: geo(0, 0), index: 1).id)
}

@Test func clusterMeansMeetAtTheAntimeridian() {
    let m = MapClustering.mean([geo(0, 179), geo(2, -179)])
    #expect(abs(abs(m.longitude) - 180) < 1e-6 && abs(m.latitude - 1) < 1e-9)
    #expect(MapClustering.mean([geo(0, 0), geo(0, 180)]).latitude == 0)
}

/// H1: 50,000 pins spread over the world stay under the marker cap at any zoom
/// and cluster within budget (off the main actor in the app).
@Test func fiftyThousandPinsClusterWithinBudgetAndUnderTheCap() {
    var pins: [MapPin] = []
    var rng = SystemRandomNumberGenerator()
    for i in 0..<50_000 {
        let p = geo(Double.random(in: -60...60, using: &rng), Double.random(in: -180...180, using: &rng))
        pins.append(i.isMultiple(of: 2) ? MapPin(photo: PhotoID(Int64(i)), location: p)
                                        : MapPin(remote: remotePin("\(i)"), location: p, index: i))
    }
    let size = CGSize(width: 1400, height: 900)
    let clock = ContinuousClock()
    var worst = Duration.zero
    for region in [world, GeoRegion(center: geo(0, 0), latitudeDelta: 20, longitudeDelta: 30), island] {
        let t = clock.now
        let c = MapClustering.clusters(pins, region: region, size: size)
        worst = max(worst, clock.now - t)
        #expect(c.count <= MapClustering.defaultLimit)
        #expect(c.reduce(0) { $0 + $1.count } <= pins.count)
    }
    #expect(MapClustering.clusters(pins, region: world, size: size).reduce(0) { $0 + $1.count } == pins.count,
            "the world view keeps every pin")
    #expect(worst < .seconds(2), "\(worst) for 50k pins (debug build)")
    // A tight cap folds cells together until it fits.
    #expect(MapClustering.clusters(pins, region: world, size: size, limit: 5).count <= 5)
}

// MARK: - The map filter

@Test func placeFilterMatchesPhotosAndAreas() {
    var a = samplePhoto(1)
    a.meta.gps = geo(22.2, -159.5)
    let b = samplePhoto(2)
    var c = samplePhoto(3)
    c.meta.gps = geo(35, 139)
    c.geotag = geo(22.2, -159.5) // the geotag wins over the file's GPS
    #expect(c.location == geo(22.2, -159.5))
    let area = PlaceFilter.area(island)
    #expect(area.matches(a) && !area.matches(b) && area.matches(c))
    #expect(area.title == "Map: area")
    let ids = PlaceFilter.photos([PhotoID(2)])
    #expect(!ids.matches(a) && ids.matches(b))
    #expect(ids.title == "Map: 1 photo" && PlaceFilter.photos([PhotoID(1), PhotoID(2)]).title == "Map: 2 photos")
    var f = PhotoFilter()
    f.place = area
    #expect(f.isActive && f.activeCriteria == 1)
    #expect(f.matches(a) && !f.matches(b))
}

@Test func remotePointsFollowTheRatingAndFlagCriteria() {
    let p = RemoteMapPoint(sessionRoot: remoteRoot, sessionName: "S", url: remoteRoot, location: geo(1, 1), rating: 3,
                           pick: .picked)
    var f = PhotoFilter()
    #expect(MapModel.remoteMatches(p, f))
    f.minRating = 4
    #expect(!MapModel.remoteMatches(p, f))
    f = PhotoFilter()
    f.unratedOnly = true
    #expect(!MapModel.remoteMatches(p, f))
    for (pick, ok) in [(PhotoFilter.PickFilter.picked, true), (.unflagged, false), (.unrejected, true), (.rejected, false)] {
        f = PhotoFilter()
        f.pick = pick
        #expect(MapModel.remoteMatches(p, f) == ok, "\(pick)")
    }
}

// MARK: - The model

@MainActor @Test func pinsFollowTheBrowserFilterButNotTheMapPart() async {
    let m = await loaded()
    let map = m.map
    #expect(Set(map.pins.compactMap(\.photo)) == Set([1, 2, 3, 6].map { PhotoID($0) }))
    #expect(map.locatedCount == 4)
    #expect(map.pins.first { $0.photo == PhotoID(6) }?.location == geo(48.86, 2.35))
    m.filter.minRating = 3
    #expect(map.pins.compactMap(\.photo) == [PhotoID(2)])
    m.filter = PhotoFilter()
    m.filter.place = .photos([PhotoID(1)])
    #expect(map.pins.count == 4, "the map part never empties the map")
    // Before the view reports its camera: the world at a default size.
    await map.refreshed()
    #expect(map.clusters.map(\.count).sorted() == [1, 1, 2])
    map.cameraChanged(island, size: CGSize(width: 800, height: 500))
    await map.refreshed()
    #expect(Set(map.clusters.flatMap(\.photoIDs)) == [PhotoID(1), PhotoID(2)], "only what's near the view")
}

@MainActor @Test func clickingAMarkerFiltersAndSelectsItsPhotos() async {
    let m = await loaded()
    let map = m.map
    map.cameraChanged(world, size: CGSize(width: 800, height: 500))
    await map.refreshed()
    let island = map.clusters.first { $0.count == 2 }!
    map.select(island)
    #expect(m.filter.place == .photos([PhotoID(1), PhotoID(2)]))
    #expect(m.visibleIDs == [PhotoID(1), PhotoID(2)])
    #expect(m.selection == [PhotoID(1), PhotoID(2)] && m.focusedID == PhotoID(1))
    #expect(map.isHighlighted(island), "selection mirrored on the map")
    let sydney = map.clusters.first { $0.photoIDs == [PhotoID(3)] }!
    #expect(!map.isHighlighted(sydney))
    #expect(map.remoteChoice.isEmpty)
    map.clearFilter()
    #expect(m.filter.place == nil && m.visibleIDs.count == 8)
    // A marker with only other sessions' photos offers them and leaves the browser alone.
    let r = remotePin()
    map.select(MapCluster(pins: [MapPin(remote: r, location: geo(1, 1), index: 0)], location: geo(1, 1)))
    #expect(map.remoteChoice == [r] && m.filter.place == nil)
}

@MainActor @Test func areasFilterByDrawingOrByTheView() async {
    let m = await loaded()
    let map = m.map
    let size = CGSize(width: 1000, height: 1000)
    let surface = FlatSurface(region: GeoRegion(south: -60, west: -180, north: 60, east: 180), size: size)
    map.cameraChanged(GeoRegion(south: -60, west: -180, north: 60, east: 180), size: size)
    // A rectangle around the Pacific's north half: the island only (dragged right to left).
    map.drawingArea = true
    map.drewArea(from: CGPoint(x: 120, y: 450), to: CGPoint(x: 10, y: 100), on: surface)
    #expect(!map.drawingArea)
    guard case .area(let drawn) = m.filter.place else { Issue.record("no area"); return }
    #expect(drawn.contains(geo(22.2, -159.5)) && !drawn.crossesAntimeridian)
    #expect(m.visibleIDs == [PhotoID(1), PhotoID(2)])
    // Off the surface: nothing changes.
    map.drewArea(from: CGPoint(x: -5, y: 0), to: CGPoint(x: 10, y: 10), on: surface)
    #expect(m.filter.place == .area(drawn))
    // M2: a box over more than half the world keeps what it covers (the island to Paris).
    map.drewArea(from: CGPoint(x: 50, y: 100), to: CGPoint(x: 600, y: 450), on: surface)
    guard case .area(let wide) = m.filter.place else { Issue.record("no area"); return }
    #expect(!wide.crossesAntimeridian && wide.longitudeSpan > 180)
    #expect(Set(m.visibleIDs) == [PhotoID(1), PhotoID(2), PhotoID(6)])
    // A box as wide as the whole world (both edges at the same longitude on a
    // zoomed-out map): every longitude, not a sliver.
    map.cameraChanged(world, size: size)
    map.drewArea(from: CGPoint(x: 0, y: 300), to: CGPoint(x: 1000, y: 600), on: WrappedSurface(size: size))
    guard case .area(let all) = m.filter.place else { Issue.record("no area"); return }
    #expect(all.longitudeSpan == 360)

    // Filter to view follows the camera, and leaves when turned off.
    map.cameraChanged(island, size: size)
    map.filterToView = true
    #expect(m.filter.place == .area(island))
    let sydney = GeoRegion(center: geo(-33.86, 151.21), latitudeDelta: 1, longitudeDelta: 1)
    map.cameraChanged(sydney, size: size)
    #expect(m.visibleIDs == [PhotoID(3)])
    map.filterToView = false
    #expect(m.filter.place == nil)
    // Clearing the browser's filter turns filter-to-view off too (L3).
    map.filterToView = true
    m.filter = PhotoFilter()
    #expect(!map.filterToView)
    map.cameraChanged(island, size: size)
    #expect(m.filter.place == nil, "a later camera move doesn't bring it back")
    // A marker click ends filter-to-view.
    map.filterToView = true
    await map.refreshed()
    let marker = map.clusters.first { $0.photoIDs.contains(PhotoID(1)) }!
    map.select(marker)
    #expect(!map.filterToView)
    #expect(m.filter.place == .photos(Set(marker.photoIDs)))
    map.filterToView = false
    #expect(m.filter.place == .photos(Set(marker.photoIDs)), "turning it off keeps a marker's filter")
}

@MainActor @Test func aSessionChangeDropsTheMapFilter() async {
    let m = await loaded()
    let map = m.map
    map.cameraChanged(island, size: CGSize(width: 400, height: 400))
    map.drawingArea = true
    m.filter.place = .photos([PhotoID(7), PhotoID(8)])
    await m.open(GeoStore.other)
    #expect(await waitUntil { m.currentSession == GeoStore.other && m.photoCount == 8 })
    #expect(m.filter.place == nil, "the old session's ids would pick unrelated photos")
    #expect(m.visibleIDs.count == 8)
    #expect(map.region == nil && !map.drawingArea && !map.filterToView)
    // Coming back to the map frames the new session.
    map.appeared()
    #expect(map.requestedRegion?.contains(geo(22.2, -159.5)) == true)
}

@MainActor @Test func droppingPhotosGeotagsThemUndoablyAndSavesTheSidecar() async {
    let store = GeoStore()
    let m = await loaded(store)
    let map = m.map
    let surface = FlatSurface(region: island, size: CGSize(width: 400, height: 400))
    // The drag carries the selection when the dragged photo is in it, with this session and instance.
    m.focus(PhotoID(4), extend: false)
    m.toggleSelection(PhotoID(5))
    let item = try! #require(map.dragItem(from: PhotoID(4)))
    #expect(item.ids == [PhotoID(4), PhotoID(5)] && item.session == GeoStore.session.id)
    #expect(item.instance == PhotoDragItem.thisInstance)
    #expect(map.dragItem(from: PhotoID(1))?.ids == [PhotoID(1)])
    #expect(map.drop(item, at: CGPoint(x: 200, y: 200), on: surface))
    let tagged = m.photo(PhotoID(4))!.geotag!
    #expect(abs(tagged.latitude - 22.2) < 1e-4 && abs(tagged.longitude - -159.5) < 1e-4)
    #expect(m.photo(PhotoID(5))!.location == tagged)
    #expect(m.photo(PhotoID(8))!.geotag == tagged, "photo 4's other variant moves with it")
    #expect(m.history.undoLabel == "Set Location of 3 Photos")
    #expect(await waitUntil { store.saved.contains { $0.id == PhotoID(4) && $0.geotag == tagged } })
    // The same place again changes nothing; off the surface does nothing; videos are skipped.
    #expect(!map.setLocation(tagged, for: [PhotoID(4)]))
    #expect(!map.drop(item, at: CGPoint(x: -1, y: 0), on: surface))
    #expect(!map.setLocation(geo(1, 2), for: [PhotoID(7)]))
    #expect(m.photo(PhotoID(7))!.geotag == nil)
    // M8: a drag from another instance or another session is refused.
    let foreign = PhotoDragItem(ids: [PhotoID(5)], session: GeoStore.session.id, instance: UUID())
    #expect(!map.drop(foreign, at: CGPoint(x: 10, y: 10), on: surface))
    let stale = PhotoDragItem(ids: [PhotoID(5)], session: GeoStore.other.id)
    #expect(!map.drop(stale, at: CGPoint(x: 10, y: 10), on: surface))
    #expect(m.photo(PhotoID(5))!.location == tagged, "untouched")
    m.undo()
    #expect(m.photo(PhotoID(4))!.geotag == nil && m.photo(PhotoID(5))!.location == nil)
    #expect(m.photo(PhotoID(8))!.geotag == nil)
    m.redo()
    #expect(m.photo(PhotoID(4))!.geotag == tagged)
    // A photo with its own GPS can be moved; removing the geotag shows the file's GPS again.
    #expect(map.setLocation(geo(1, 2), for: [PhotoID(1)]))
    #expect(m.history.undoLabel == "Set Location")
    m.focus(PhotoID(1), extend: false)
    #expect(map.removableSelection == [PhotoID(1)])
    map.removeLocation(of: [PhotoID(1), PhotoID(3)])
    #expect(m.photo(PhotoID(1))!.location == geo(22.2, -159.5))
    #expect(m.history.undoLabel == "Remove Location")
    #expect(map.removableSelection.isEmpty)
    // Export writes where the photo was taken: the geotag, else the file's position.
    m.focus(PhotoID(6), extend: false)
    #expect(m.exportItems.first?.location == geo(48.86, 2.35))
    m.focus(PhotoID(1), extend: false)
    #expect(m.exportItems.first?.location == geo(22.2, -159.5))
    m.focus(PhotoID(5), extend: false)
    #expect(m.exportItems.first?.location == tagged)
}

@MainActor @Test func cameraRequestsFrameRememberAndFollowTheBrowser() async {
    let m = await loaded()
    let map = m.map
    map.appeared() // first time: every marker framed
    #expect(map.cameraRequest == 1)
    let all = map.requestedRegion!
    #expect(all.contains(geo(22.2, -159.5)) && all.contains(geo(-33.86, 151.21)) && all.contains(geo(48.86, 2.35)))
    m.focus(PhotoID(3), extend: false)
    map.showSelection()
    #expect(map.cameraRequest == 2 && map.requestedRegion!.contains(geo(-33.86, 151.21)))
    #expect(map.requestedRegion!.latitudeSpan >= 0.2, "never closer than a city")
    m.focus(PhotoID(4), extend: false) // no position: nothing to frame
    map.showSelection()
    #expect(map.cameraRequest == 2)
    // Focus follows only when the photo is off screen.
    map.focusChanged()
    #expect(map.cameraRequest == 2, "no camera yet")
    map.cameraChanged(island, size: CGSize(width: 400, height: 400))
    m.focus(PhotoID(1), extend: false)
    map.focusChanged()
    #expect(map.cameraRequest == 2, "already in view")
    m.focus(PhotoID(3), extend: false)
    map.focusChanged()
    #expect(map.cameraRequest == 3 && map.requestedRegion!.contains(geo(-33.86, 151.21)))
    #expect(abs(map.requestedRegion!.latitudeSpan - island.latitudeSpan) < 1e-9, "same zoom")
    // L9: coming back to Map mode keeps the camera where the user left it.
    map.cameraChanged(island, size: CGSize(width: 400, height: 400))
    map.appeared()
    #expect(map.requestedRegion == island)
}

@MainActor @Test func tilesAreAskedForOnceAndCanBeTurnedOff() async {
    let m = await loaded()
    let map = m.map
    m.settings.mapTiles = nil
    #expect(map.tiles == .ask)
    #expect(MapModel.tilesNotice.contains("Apple Maps") && MapModel.tilesNotice.contains("aren't uploaded"))
    map.allowTiles(false)
    #expect(map.tiles == .off && m.settings.mapTiles == false)
    map.allowTiles(true)
    #expect(map.tiles == .on)
    // Stored in the preferences.
    #expect(AppSettings(defaults: m.settings.userDefaults).mapTiles == true)
    m.settings.mapTiles = nil
    #expect(AppSettings(defaults: m.settings.userDefaults).mapTiles == nil)
}

@MainActor @Test func allSessionsAddsOtherSessionsPhotos() async {
    let m = await loaded()
    let map = m.map
    let other = RemoteMapPoint(sessionRoot: URL(fileURLWithPath: "/pics/Home"), sessionName: "Home",
                               url: URL(fileURLWithPath: "/pics/Home/h.ARW"), location: geo(40.7, -74), rating: 5)
    let other2 = RemoteMapPoint(sessionRoot: URL(fileURLWithPath: "/pics/Alps"), sessionName: "Alps",
                                url: URL(fileURLWithPath: "/pics/Alps/a.ARW"), location: geo(46, 8))
    let mine = RemoteMapPoint(sessionRoot: GeoStore.session.url, sessionName: "Island",
                              url: URL(fileURLWithPath: "/tmp/island/DSC01.ARW"), location: geo(22.2, -159.5))
    let source = FakeMapSource([other, other2, mine])
    map.configure(source: source)
    await map.reloadRemote()
    #expect(source.calls == 0 && map.remote.isEmpty, "This Session reads no index")
    map.scope = .allSessions
    #expect(await waitUntil { map.remote.count == 2 })
    #expect(map.loading == .idle)
    #expect(map.pins.count == 6, "the open session's own photos come from the browser")
    #expect(Set(map.pins.compactMap(\.remote?.sessionName)) == ["Home", "Alps"])
    m.filter.minRating = 4 // L4: the browser's criteria apply to other sessions too
    #expect(map.pins.compactMap(\.remote?.sessionName) == ["Home"])
    m.filter = PhotoFilter()
    let pins = map.pins.compactMap(\.remote)
    #expect(MapModel.sessions(pins + pins).map(\.name) == ["Alps", "Home"])
    #expect(MapModel.sessions(pins + pins).map(\.count) == [2, 2])
    map.scope = .session
    #expect(await waitUntil { map.remote.isEmpty })

    let failing = FakeMapSource(error: MockError.invalid("index busy"))
    map.configure(source: failing)
    map.scope = .allSessions
    #expect(await waitUntil { if case .failed = map.loading { true } else { false } })
    #expect(map.remote.isEmpty)
}

@MainActor @Test func openingOtherSessionsPhotosShowsThem() async {
    let m = await loaded()
    let map = m.map
    // Demo mode can't open folders (a toast says so); the photos are then matched in
    // whatever session is open, here the same one.
    let pin = MapPin.Remote(sessionRoot: GeoStore.session.url, sessionName: "Island",
                            url: URL(fileURLWithPath: "/tmp/island/DSC03.ARW"))
    map.select(MapCluster(pins: [MapPin(remote: pin, location: geo(-33.86, 151.21), index: 0)],
                          location: geo(-33.86, 151.21)))
    await map.open(map.remoteChoice)
    #expect(map.remoteChoice.isEmpty)
    #expect(m.filter.place == .photos([PhotoID(3)]) && m.focusedID == PhotoID(3))
    // Nothing matched: no id filter is left behind for the wrong session.
    await map.open([MapPin.Remote(sessionRoot: GeoStore.session.url, sessionName: "Island",
                                  url: URL(fileURLWithPath: "/elsewhere/x.ARW"))])
    #expect(m.filter.place == nil)
    await map.open([])
}

// MARK: - Rust mapping and the FFI

@Test func positionsCrossTheFFI() {
    let p = GeoPosition(latitude: -33.5, longitude: -70.25, altitudeM: -12)
    let g = GeoPoint(p)!
    #expect(g.latitude == -33.5 && g.longitude == -70.25 && g.altitude == -12)
    #expect(g.record == p)
    #expect(RustExportService.gps(g) == ExportGps(latitude: -33.5, longitude: -70.25, altitudeM: -12))
    #expect(GeoPoint(GeoPosition(latitude: 100, longitude: 0, altitudeM: nil)) == nil)
    var raw = RawMetadata(make: "SONY", model: "ILCE-9", lens: nil, iso: nil, shutterSeconds: nil, shutterDisplay: nil,
                          aperture: nil, focalLength: nil, focalLength35mm: nil, captureTime: nil, orientation: 1,
                          width: 6000, height: 4000, serial: nil, shutterCount: nil, afX: nil, afY: nil, shotNumber: nil,
                          sequenceImageNumber: nil, gps: p)
    let photo = RustRaw.makePhoto(raw, url: URL(fileURLWithPath: "/x/A.ARW"), id: PhotoID(1), fileSize: 1,
                                  fallbackDate: Date())
    #expect(photo.meta.gps == g && photo.location == g)
    raw.gps = nil
    #expect(RustRaw.makePhoto(raw, url: URL(fileURLWithPath: "/x/A.ARW"), id: PhotoID(1), fileSize: 1,
                              fallbackDate: Date()).location == nil)
    let rec = MapPointRecord(sessionRoot: "/s", sessionName: "S", path: "/s/a.ARW", latitude: 1, longitude: 2,
                             captureTimeMs: nil, rating: 4, pick: .rejected)
    let point = RustMapPointSource.point(rec)!
    #expect(point.url.path == "/s/a.ARW" && point.rating == 4 && point.pick == .rejected && point.location == geo(1, 2))
    #expect(RustMapPointSource.point(MapPointRecord(sessionRoot: "/s", sessionName: "S", path: "/s/b.ARW", latitude: 95,
                                                    longitude: 2, captureTimeMs: nil, rating: 0,
                                                    pick: .unflagged)) == nil)
}

/// N1: once the places scan ran, a photo's EXIF position keeps its altitude (so
/// exports keep it); the scanned position only fills in XMP-only photos.
@Test func theScannedPositionNeverReplacesTheCameraAltitude() {
    let exif = GeoPosition(latitude: 46.5, longitude: 8.0, altitudeM: 2400)
    let scanned = GeoPosition(latitude: 46.5, longitude: 8.0, altitudeM: nil)
    let raw = RawMetadata(make: "SONY", model: "ILCE-9", lens: nil, iso: nil, shutterSeconds: nil, shutterDisplay: nil,
                          aperture: nil, focalLength: nil, focalLength35mm: nil, captureTime: nil, orientation: 1,
                          width: 6000, height: 4000, serial: nil, shutterCount: nil, afX: nil, afY: nil, shotNumber: nil,
                          sequenceImageNumber: nil, gps: exif)
    var r = VariantRecord(variantId: 1, imageId: 1, originalPath: "/x/A.ARW", fileName: "A.ARW", name: "", rating: 0,
                          colorTag: .none, pick: .unflagged, edited: false, readOnly: false, missing: false,
                          fileChanged: false, keptInStore: false, fileSize: 1, fileSequence: nil, captureTimeMs: nil,
                          width: 6000, height: 4000, meta: raw)
    r.fileGps = scanned
    let p = RustSessionStore.photo(from: r)
    #expect(p.location?.altitude == 2400)
    #expect(RustExportService.gps(p.location!).altitudeM == 2400, "what the export writes")
    // XMP only (no EXIF position): the scanned one.
    var xmpOnly = r
    xmpOnly.meta = nil
    xmpOnly.fileGps = GeoPosition(latitude: 10.5, longitude: -20, altitudeM: nil)
    #expect(RustSessionStore.photo(from: xmpOnly).location == geo(10.5, -20))
}

@Test func writePlanWritesChangedGeotags() {
    var a = samplePhoto(1)
    let b = samplePhoto(2)
    var c = samplePhoto(3)
    c.geotag = geo(1, 2)
    #expect(RustSessionStore.writePlan([b], stored: [:]).geotags.isEmpty, "a new photo without one writes none")
    var a2 = a
    a2.geotag = geo(1, 2)
    let plan = RustSessionStore.writePlan([a2, c], stored: [a.id: a])
    #expect(plan.geotags[geo(1, 2)] == [1, 3])
    a.geotag = geo(1, 2)
    var removed = a
    removed.geotag = nil
    #expect(RustSessionStore.writePlan([removed], stored: [a.id: a]).geotags[nil] == [1])
    #expect(RustSessionStore.writePlan([a], stored: [a.id: a]) == RustSessionStore.WritePlan())
}

/// The real FFI against a global index with rows in it (written the way
/// `sync_find` writes them): valid points mapped, out-of-range ones dropped.
@Test func rustMapSourceReadsTheGlobalIndex() async throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("map-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let db = dir.appendingPathComponent("global.db")
    let source = RustMapPointSource(globalIndex: db)
    #expect(try await source.points().isEmpty) // creates the index
    var handle: OpaquePointer?
    #expect(sqlite3_open(db.path, &handle) == SQLITE_OK)
    sqlite3_busy_timeout(handle, 5000)
    let sql = """
        INSERT INTO sessions(id, root, name) VALUES (1, '/pics/Island', 'Island');
        INSERT INTO find_items(session_id, path, key, capture_ms, rating, pick, keywords, people, suggested, location,
            rejected, lat, lon) VALUES
            (1, '/pics/Island/a.ARW', NULL, 5, 3, 1, '', '', '', '', '', 22.2, -159.5),
            (1, '/pics/Island/b.ARW', NULL, 6, 0, 0, '', '', '', '', '', NULL, NULL),
            (1, '/pics/Island/c.ARW', NULL, 7, 0, 0, '', '', '', '', '', 95.0, 0.0);
        UPDATE search_generation SET n = n + 1;
        """
    #expect(sqlite3_exec(handle, sql, nil, nil, nil) == SQLITE_OK, "\(String(cString: sqlite3_errmsg(handle)))")
    sqlite3_close(handle)
    let points = try await source.points()
    #expect(points.count == 1)
    #expect(points.first?.sessionName == "Island" && points.first?.url.path == "/pics/Island/a.ARW")
    #expect(points.first?.location == geo(22.2, -159.5) && points.first?.rating == 3 && points.first?.pick == .picked)
    await #expect(throws: (any Error).self) {
        _ = try await RustMapPointSource(globalIndex: dir).points() // a folder isn't a database
    }
}

/// HEIC goes through the app's encoder: the photo's position replaces the
/// file's in its EXIF, and "Remove location" strips it (JPEG alongside).
@MainActor @Test func exportsWriteThePhotosPositionInHEICAndJPEG() async throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("map-exp-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: dir) }
    let src = dir.appendingPathComponent("IMG_0001.HEIC")
    let ctx = CGContext(data: nil, width: 64, height: 48, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
    ctx.setFillColor(CGColor(gray: 0.5, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: 64, height: 48))
    let dest = try #require(CGImageDestinationCreateWithURL(src as CFURL, UTType.heic.identifier as CFString, 1, nil))
    let gpsIn: [CFString: Any] = [kCGImagePropertyGPSLatitude: 35.68, kCGImagePropertyGPSLatitudeRef: "N",
                                  kCGImagePropertyGPSLongitude: 139.69, kCGImagePropertyGPSLongitudeRef: "E"]
    CGImageDestinationAddImage(dest, try #require(ctx.makeImage()),
                               [kCGImagePropertyGPSDictionary: gpsIn,
                                kCGImagePropertyTIFFDictionary: [kCGImagePropertyTIFFModel: "iPhone 17 Pro"]] as CFDictionary)
    #expect(CGImageDestinationFinalize(dest))
    let item = ExportJobItem(photoID: PhotoID(1), url: src, seq: 1, location: geo(22.25, -159.5, -3))
    let recipes = [ExportRecipe(name: "HEIC", format: .heic, subfolder: "heic"),
                   ExportRecipe(name: "JPEG", format: .jpeg, subfolder: "jpeg"),
                   ExportRecipe(name: "NoGPS", format: .heic, subfolder: "nogps", stripGPS: true)]
    let o = try await RustExportService().export(ExportJob(items: [item], recipes: recipes,
                                                           defaultFolder: dir.appendingPathComponent("Out"))) { _ in }
    #expect(o.failures.isEmpty, "\(o.failures)")
    #expect(o.written.count == 3)
    for w in o.written {
        let props = CGImageSourceCreateWithURL(w.url as CFURL, nil)
            .flatMap { CGImageSourceCopyPropertiesAtIndex($0, 0, nil) } as? [CFString: Any]
        let gps = props?[kCGImagePropertyGPSDictionary] as? [CFString: Any]
        if w.recipeName == "NoGPS" {
            #expect(gps == nil, "location removed")
        } else {
            #expect((gps?[kCGImagePropertyGPSLatitude] as? Double).map { abs($0 - 22.25) < 1e-4 } == true, "\(w.recipeName)")
            #expect(gps?[kCGImagePropertyGPSLongitudeRef] as? String == "W", "\(w.recipeName)")
            #expect(gps?[kCGImagePropertyGPSAltitudeRef] as? Int == 1, "\(w.recipeName): below sea level")
        }
    }
}

@Test func photoDragsCarryIDsSessionAndInstance() throws {
    let s = UUID()
    let item = PhotoDragItem(ids: [PhotoID(4), PhotoID(5)], session: s)
    let data = try JSONEncoder().encode(item)
    let back = try JSONDecoder().decode(PhotoDragItem.self, from: data)
    #expect(back == item && back.session == s && back.instance == PhotoDragItem.thisInstance)
    #expect(UTType.darkslidePhotos.identifier == "casa.baillargeon.darkslide.photos")
}
