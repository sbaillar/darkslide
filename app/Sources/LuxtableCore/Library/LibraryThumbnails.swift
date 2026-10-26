import CoreGraphics
import LuxtableFFI
import Foundation

/// [library] Byte-bounded thumbnail cache for the Library grid. A hit is
/// synchronous (no placeholder flash when scrolling back); a load runs in the
/// cell's task (cancelled when the cell leaves the screen) and decodes off the
/// main actor; `prefetch` warms the rows below the viewport at low priority.
/// Entries are keyed by photo, size bucket and edit, so a re-edit shows anew.
@MainActor
public final class LibraryThumbnails {
    private final class Entry {
        let image: CGImage
        init(_ image: CGImage) { self.image = image }
    }

    var backend: (any LibraryBackend)?
    private let cache = NSCache<NSString, Entry>()
    private var prefetchTask: Task<Void, Never>?
    public private(set) var loads = 0

    /// `costLimit` in bytes of decoded RGBA (default 256 MB ≈ 1,000 thumbnails at 256 px).
    public init(costLimit: Int = 256 << 20) {
        cache.totalCostLimit = costLimit
    }

    static func key(_ item: LibraryItemRecord, _ bucket: Int) -> NSString {
        "\(item.id)@\(bucket)#\(item.editHash ?? "")" as NSString
    }

    public func cached(_ item: LibraryItemRecord, bucket: Int) -> CGImage? {
        cache.object(forKey: Self.key(item, bucket))?.image
    }

    /// Cached image or a fresh load; `nil` when cancelled or unreadable (offline).
    public func image(for item: LibraryItemRecord, bucket: Int) async -> CGImage? {
        if let hit = cached(item, bucket: bucket) { return hit }
        guard let backend else { return nil }
        loads += 1
        guard let img = try? await backend.thumbnail(for: item, maxPixelSize: bucket), !Task.isCancelled else { return nil }
        cache.setObject(Entry(img), forKey: Self.key(item, bucket), cost: img.bytesPerRow * img.height)
        return img
    }

    /// Load `items` one after another in the background (a newer prefetch replaces it).
    public func prefetch(_ items: [LibraryItemRecord], bucket: Int) {
        prefetchTask?.cancel()
        let todo = items.filter { cached($0, bucket: bucket) == nil }
        guard !todo.isEmpty else { return }
        prefetchTask = Task(priority: .utility) { [weak self] in
            for item in todo {
                guard !Task.isCancelled else { return }
                _ = await self?.image(for: item, bucket: bucket)
            }
        }
    }

    /// Wait for the prefetch (tests).
    func prefetched() async { await prefetchTask?.value }

    public func removeAll() { cache.removeAllObjects() }
}

/// [library] The Dates tree and the capture-time range of a date scope. Capture
/// times are the camera's clock read as UTC (as the index stores them).
public enum LibraryDates {
    public struct Month: Hashable, Sendable {
        public var month: UInt32
        public var count: UInt64
        public var days: [LibraryDayRecord]
    }

    public struct Year: Hashable, Sendable {
        public var year: Int32
        public var count: UInt64
        public var months: [Month]
    }

    /// Years, months and days with photos, newest first.
    public static func tree(_ days: [LibraryDayRecord]) -> [Year] {
        var years: [Year] = []
        for d in days.sorted(by: { ($0.year, $0.month, $0.day) > ($1.year, $1.month, $1.day) }) {
            if years.last?.year != d.year { years.append(Year(year: d.year, count: 0, months: [])) }
            years[years.count - 1].count += d.count
            if years[years.count - 1].months.last?.month != d.month {
                years[years.count - 1].months.append(Month(month: d.month, count: 0, days: []))
            }
            let y = years.count - 1, m = years[y].months.count - 1
            years[y].months[m].count += d.count
            years[y].months[m].days.append(d)
        }
        return years
    }

    private static let utc: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "UTC")!
        c.locale = Locale(identifier: "en_US_POSIX")
        return c
    }()

    static func ms(_ y: Int32, _ m: UInt32, _ d: UInt32) -> Int64 {
        let date = utc.date(from: DateComponents(year: Int(y), month: Int(m), day: Int(d)))!
        return Int64((date.timeIntervalSince1970 * 1000).rounded())
    }

    /// `[from, to)` in capture milliseconds for a year, month or day scope.
    public static func range(_ scope: LibraryModel.Scope) -> (Int64, Int64)? {
        switch scope {
        case .year(let y): (ms(y, 1, 1), ms(y + 1, 1, 1))
        case .month(let y, let m): (ms(y, m, 1), m == 12 ? ms(y + 1, 1, 1) : ms(y, m + 1, 1))
        case .day(let y, let m, let d): (ms(y, m, d), ms(y, m, d) + 86_400_000)
        default: nil
        }
    }

    public static func monthName(_ m: UInt32) -> String {
        let names = utc.monthSymbols
        return (1...12).contains(m) ? names[Int(m) - 1] : "\(m)"
    }

    /// "2024", "October 2024", "October 5, 2024".
    public static func title(_ scope: LibraryModel.Scope) -> String {
        switch scope {
        case .year(let y): "\(y)"
        case .month(let y, let m): "\(monthName(m)) \(y)"
        case .day(let y, let m, let d): "\(monthName(m)) \(d), \(y)"
        default: ""
        }
    }
}
