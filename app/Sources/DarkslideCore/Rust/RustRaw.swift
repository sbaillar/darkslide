import CoreGraphics
import DarkslideFFI
import Foundation

/// RAW files through `ds-raw`: metadata for the browser, and support checks.
public enum RustRaw {
    /// The RAW extensions ds-raw reads, lowercase (what a session open lists).
    public static let extensions: Set<String> = Set(rawExtensions())

    /// Extension + magic check (cheap: one 16-byte read).
    /// `false` also if the check itself failed (it can't throw into callers
    /// that only filter files; C3: never `try!`).
    public static func isSupported(_ url: URL) -> Bool {
        (try? isSupportedRaw(path: url.path)) ?? false
    }

    /// Read capture metadata and build the browser's `Photo` (unrated, unedited).
    /// Throws `RenderError` (`.io`, `.unsupported`, …) or `CancellationError`.
    public static func photo(at url: URL, id: PhotoID) async throws -> Photo {
        let token = try CancelToken()
        let meta: RawMetadata
        do {
            meta = try await withTaskCancellationHandler {
                try await rawMetadata(path: url.path, token: token)
            } onCancel: {
                try? token.cancel()
            }
        } catch let e as DsError {
            if case .Cancelled = e { throw CancellationError() }
            throw RenderError(e)
        }
        let attrs = try? FileManager.default.attributesOfItem(atPath: url.path)
        return makePhoto(meta, url: url, id: id,
                         fileSize: (attrs?[.size] as? NSNumber)?.int64Value ?? 0,
                         fallbackDate: attrs?[.modificationDate] as? Date ?? Date(timeIntervalSince1970: 0))
    }

    /// Pure mapping from the FFI record (unit-tested).
    static func makePhoto(_ m: RawMetadata, url: URL, id: PhotoID, fileSize: Int64, fallbackDate: Date,
                          timeZone: TimeZone = .current) -> Photo {
        var meta = PhotoMeta(
            camera: cameraName(make: m.make, model: m.model),
            lens: m.lens ?? "",
            iso: Int(m.iso ?? 0),
            shutter: m.shutterSeconds ?? 0,
            aperture: Double(m.aperture ?? 0),
            focalLength: Double(m.focalLength ?? 0),
            captureDate: captureDate(m.captureTime, fallback: fallbackDate, timeZone: timeZone),
            pixelWidth: Int(m.width ?? 0),
            pixelHeight: Int(m.height ?? 0),
            fileSize: fileSize)
        meta.captureWallClockMs = m.captureTime.flatMap(wallClockMs)
        meta.gps = m.gps.flatMap(GeoPoint.init) // [map]
        return Photo(id: id, filename: url.lastPathComponent, meta: withBurstInfo(meta, m, url: url), url: url)
    }

    /// The camera's clock reading as ms read as UTC (ds-ffi `capture_ms`).
    static func wallClockMs(_ t: CaptureTimeData) -> Int64? {
        var c = DateComponents()
        (c.year, c.month, c.day) = (Int(t.year), Int(t.month), Int(t.day))
        (c.hour, c.minute, c.second) = (Int(t.hour), Int(t.minute), Int(t.second))
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(secondsFromGMT: 0)!
        guard let d = cal.date(from: c) else { return nil }
        return Int64((d.timeIntervalSince1970 * 1000).rounded()) + Int64(t.nanos / 1_000_000)
    }

    /// Shot number, file-name sequence and AF point (ds-ai grouping/culling inputs).
    static func withBurstInfo(_ meta: PhotoMeta, _ m: RawMetadata, url: URL) -> PhotoMeta {
        var meta = meta
        meta.shotNumber = m.shotNumber.map(Int.init)
        meta.fileSequence = ((try? filenameSequence(path: url.path)) ?? nil).map { Int(clamping: $0) }
        if let x = m.afX, let y = m.afY { meta.afPoint = CGPoint(x: Double(x), y: Double(y)) }
        return meta
    }

    /// "SONY" + "ILCE-9" → "Sony ILCE-9"; a model that already names the make keeps it once.
    static func cameraName(make: String, model: String) -> String {
        let mk = make.trimmingCharacters(in: .whitespaces)
        let md = model.trimmingCharacters(in: .whitespaces)
        let pretty = mk == mk.uppercased() && mk.count > 3 ? mk.capitalized : mk
        if pretty.isEmpty { return md }
        if md.lowercased().hasPrefix(pretty.lowercased()) { return md }
        return md.isEmpty ? pretty : "\(pretty) \(md)"
    }

    /// Camera wall-clock time → `Date`, using the recorded UTC offset when present,
    /// otherwise `timeZone` (the camera clock is assumed to be local time).
    static func captureDate(_ t: CaptureTimeData?, fallback: Date, timeZone: TimeZone) -> Date {
        guard let t else { return fallback }
        var c = DateComponents()
        (c.year, c.month, c.day) = (Int(t.year), Int(t.month), Int(t.day))
        (c.hour, c.minute, c.second, c.nanosecond) = (Int(t.hour), Int(t.minute), Int(t.second), Int(t.nanos))
        c.timeZone = t.utcOffsetMinutes.flatMap { TimeZone(secondsFromGMT: Int($0) * 60) } ?? timeZone
        return Calendar(identifier: .gregorian).date(from: c) ?? fallback
    }
}

extension GeoPoint {
    /// [map] A position from Rust (`nil` if out of range).
    init?(_ g: GeoPosition) {
        self.init(latitude: g.latitude, longitude: g.longitude, altitude: g.altitudeM)
    }

    /// [map] The position for Rust.
    var record: GeoPosition { GeoPosition(latitude: latitude, longitude: longitude, altitudeM: altitude) }
}
