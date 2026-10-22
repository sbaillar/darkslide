import CoreGraphics
import Foundation

/// Where a photo was taken (WGS84), decimal degrees: negative latitude = south,
/// negative longitude = west; altitude in metres (negative = below sea level).
public struct GeoPoint: Hashable, Sendable, Codable {
    public let latitude: Double
    public let longitude: Double
    public let altitude: Double?

    /// `nil` when a coordinate is out of range or not finite (a non-finite altitude is dropped).
    public init?(latitude: Double, longitude: Double, altitude: Double? = nil) {
        guard latitude.isFinite, longitude.isFinite, abs(latitude) <= 90, abs(longitude) <= 180 else { return nil }
        self.latitude = latitude
        self.longitude = longitude
        self.altitude = altitude.flatMap { $0.isFinite ? $0 : nil }
    }

    /// "22.2000° N, 159.5000° W".
    public var text: String {
        let lat = String(format: "%.4f° %@", abs(latitude), latitude < 0 ? "S" : "N")
        let lon = String(format: "%.4f° %@", abs(longitude), longitude < 0 ? "W" : "E")
        return "\(lat), \(lon)"
    }
}

/// A latitude/longitude box. `west > east` means it crosses the antimeridian.
public struct GeoRegion: Hashable, Sendable {
    public var south: Double
    public var west: Double
    public var north: Double
    public var east: Double

    public init(south: Double, west: Double, north: Double, east: Double) {
        self.south = min(max(south, -90), 90)
        self.north = min(max(north, -90), 90)
        self.west = Self.wrap(west)
        self.east = Self.wrap(east)
    }

    /// A map's visible region (MapKit's center and span).
    public init(center: GeoPoint, latitudeDelta: Double, longitudeDelta: Double) {
        let dLon = min(abs(longitudeDelta), 360) / 2
        self.init(south: center.latitude - abs(latitudeDelta) / 2, west: dLon >= 180 ? -180 : center.longitude - dLon,
                  north: center.latitude + abs(latitudeDelta) / 2, east: dLon >= 180 ? 180 : center.longitude + dLon)
    }

    /// The box a rectangle drawn on the map spans: west is its screen-left
    /// corner, east its screen-right one (so it crosses the antimeridian
    /// exactly when the left corner is east of the right one).
    public init(left: GeoPoint, right: GeoPoint) {
        self.init(south: min(left.latitude, right.latitude), west: left.longitude,
                  north: max(left.latitude, right.latitude), east: right.longitude)
    }

    /// The smallest box holding every point (across the antimeridian when that
    /// is narrower), grown by `padding` (a fraction of its size) and at least
    /// `minSpan` degrees each way; `nil` for no points.
    public static func enclosing(_ points: [GeoPoint], padding: Double = 0.15, minSpan: Double = 0.2) -> GeoRegion? {
        guard !points.isEmpty else { return nil }
        let s = points.map(\.latitude).min()!, n = points.map(\.latitude).max()!
        // The narrowest longitude range: everything but the widest gap between neighbours.
        let lons = points.map(\.longitude).sorted()
        var (w, e) = (lons.first!, lons.last!)
        var gap = 360 - (e - w) // the gap across the antimeridian
        for (a, b) in zip(lons, lons.dropFirst()) where b - a > gap {
            gap = b - a
            (w, e) = (b, a)
        }
        let lonSpan = 360 - gap
        let padLat = max((n - s) * padding, (minSpan - (n - s)) / 2, 0)
        let padLon = max(lonSpan * padding, (minSpan - lonSpan) / 2, 0)
        if lonSpan + 2 * padLon >= 360 {
            return GeoRegion(south: s - padLat, west: -180, north: n + padLat, east: 180)
        }
        // A box that doesn't cross stays inside ±180 (padding doesn't wrap it); one
        // that crosses wraps on its own.
        let (west, east) = w <= e ? (max(w - padLon, -180), min(e + padLon, 180)) : (w - padLon, e + padLon)
        return GeoRegion(south: s - padLat, west: west, north: n + padLat, east: east)
    }

    public var crossesAntimeridian: Bool { west > east }
    public var latitudeSpan: Double { north - south }
    public var longitudeSpan: Double { crossesAntimeridian ? 360 - west + east : east - west }

    public var center: GeoPoint {
        var lon = west + longitudeSpan / 2
        if lon > 180 { lon -= 360 }
        return GeoPoint(latitude: (south + north) / 2, longitude: lon)! // clamped and wrapped above
    }

    /// Grown by `screens` of its own size on every side (the whole world once that wraps).
    public func expanded(by screens: Double) -> GeoRegion {
        let dLat = latitudeSpan * screens, dLon = longitudeSpan * screens
        let wholeLon = longitudeSpan + 2 * dLon >= 360
        return GeoRegion(south: south - dLat, west: wholeLon ? -180 : west - dLon,
                         north: north + dLat, east: wholeLon ? 180 : east + dLon)
    }

    public func contains(_ p: GeoPoint) -> Bool {
        guard p.latitude >= south, p.latitude <= north else { return false }
        return crossesAntimeridian ? (p.longitude >= west || p.longitude <= east)
            : (p.longitude >= west && p.longitude <= east)
    }

    static func wrap(_ lon: Double) -> Double {
        guard lon.isFinite else { return 0 }
        if lon >= -180, lon <= 180 { return lon }
        let r = (lon + 180).truncatingRemainder(dividingBy: 360)
        return (r < 0 ? r + 360 : r) - 180
    }
}

/// Web Mercator (what map tiles use) on the unit square: x 0…1 west→east,
/// y 0…1 north→south. Latitudes beyond ±85.05° are clamped.
public enum WebMercator {
    public static let maxLatitude = 85.051_128_78

    public static func project(_ p: GeoPoint) -> CGPoint {
        let lat = min(max(p.latitude, -maxLatitude), maxLatitude) * .pi / 180
        let x = (p.longitude + 180) / 360
        let y = (1 - log(tan(lat) + 1 / cos(lat)) / .pi) / 2
        return CGPoint(x: x, y: y)
    }

    public static func unproject(_ q: CGPoint) -> GeoPoint {
        let x = q.x - floor(q.x) // wrap around the world
        let y = min(max(q.y, 0), 1)
        let lat = atan(sinh(.pi * (1 - 2 * y))) * 180 / .pi
        return GeoPoint(latitude: lat, longitude: x * 360 - 180)!
    }
}

/// A photo on the map: one in the open session (`photo`), or one of another
/// session from the global index (`remote`).
public struct MapPin: Hashable, Sendable, Identifiable {
    public struct Remote: Hashable, Sendable {
        public var sessionRoot: URL
        public var sessionName: String
        public var url: URL
    }

    public let photo: PhotoID?
    public let remote: Remote?
    public let location: GeoPoint
    /// Stable, unique within one map (`ForEach` identity).
    public let id: String
    /// Cheap sort key: the variant id, or minus (1 + the remote point's index).
    public let key: Int64

    public init(photo: PhotoID, location: GeoPoint) {
        self.photo = photo
        remote = nil
        self.location = location
        id = "p\(photo.raw)"
        key = photo.raw
    }

    /// `index`: the point's position in the remote list (a stable order).
    public init(remote: Remote, location: GeoPoint, index: Int) {
        photo = nil
        self.remote = remote
        self.location = location
        id = "r\(remote.sessionRoot.path)|\(remote.url.path)"
        key = -Int64(index) - 1
    }
}

/// Pins close together on screen, shown as one marker.
public struct MapCluster: Hashable, Sendable, Identifiable {
    public var pins: [MapPin]
    /// The members' mean position (where the marker sits).
    public var location: GeoPoint

    public var id: String { pins.first?.id ?? "" }
    public var key: Int64 { pins.first?.key ?? 0 }
    public var count: Int { pins.count }
    /// Members in the open session.
    public var photoIDs: [PhotoID] { pins.compactMap(\.photo) }
    public var remotePins: [MapPin.Remote] { pins.compactMap(\.remote) }
}

/// Screen-space grid clustering: pins whose projected positions share a
/// `cell`-point square at the map's current scale form one cluster. Only pins
/// within one screen of the visible region count (what a pan reaches first),
/// and at most `limit` markers come back: the cells grow until they fit.
public enum MapClustering {
    public static let defaultLimit = 500

    private struct Cell: Hashable {
        let x: Int
        let y: Int
    }

    /// Clusters come back largest first, then by key, so the order is stable.
    public static func clusters(_ pins: [MapPin], region: GeoRegion, size: CGSize, cell: CGFloat = 56,
                                limit: Int = defaultLimit) -> [MapCluster] {
        let near = region.expanded(by: 1)
        let shown = pins.filter { near.contains($0.location) }
        guard !shown.isEmpty else { return [] }
        let span = max(region.longitudeSpan, 1e-9)
        // Points per world width at this zoom (Mercator x is linear in longitude).
        let world = Double(max(size.width, 1)) * 360 / span
        let projected = shown.map { WebMercator.project($0.location) }
        var cellSize = Double(max(cell, 1))
        while true {
            let around = max(1, Int(min((world / cellSize).rounded(.up), Double(Int.max / 4))))
            var buckets: [Cell: [Int]] = [:]
            for (i, q) in projected.enumerated() {
                let x = Int(min(Double(q.x) * world / cellSize, Double(Int.max / 4))) % around
                let y = Int(min(Double(q.y) * world / cellSize, Double(Int.max / 4)))
                buckets[Cell(x: x, y: y), default: []].append(i)
            }
            if buckets.count <= max(limit, 1) || around == 1 {
                return buckets.values.map { members in
                    let pins = members.map { shown[$0] }.sorted { $0.key < $1.key }
                    return MapCluster(pins: pins, location: mean(pins.map(\.location)))
                }
                .sorted { $0.count != $1.count ? $0.count > $1.count : $0.key < $1.key }
            }
            cellSize *= 2
        }
    }

    /// Mean position, averaging longitude on the circle (so pins either side of
    /// the antimeridian meet there, not at 0°).
    static func mean(_ points: [GeoPoint]) -> GeoPoint {
        let lat = points.map(\.latitude).reduce(0, +) / Double(points.count)
        let x = points.map { cos($0.longitude * .pi / 180) }.reduce(0, +)
        let y = points.map { sin($0.longitude * .pi / 180) }.reduce(0, +)
        let lon = (x == 0 && y == 0) ? points[0].longitude : atan2(y, x) * 180 / .pi
        return GeoPoint(latitude: lat, longitude: lon) ?? points[0]
    }
}

/// The browser's map filter: the photos of a clicked marker, or an area.
public enum PlaceFilter: Hashable, Sendable {
    case photos(Set<PhotoID>)
    case area(GeoRegion)

    public func matches(_ p: Photo) -> Bool {
        switch self {
        case .photos(let ids): ids.contains(p.id)
        case .area(let r): p.location.map(r.contains) ?? false
        }
    }

    /// The filter bar's chip.
    public var title: String {
        switch self {
        case .photos(let ids): "Map: \(ids.count) photo\(ids.count == 1 ? "" : "s")"
        case .area: "Map: area"
        }
    }
}
