import Foundation

/// The operations the benchmark times, the same for every app (docs/BENCHMARKS.md).
public enum BenchOperation: String, Codable, CaseIterable, Sendable {
    /// Add N RAWs to a fresh session/catalog in place, until every preview is built.
    case importFiles = "import"
    /// Open an existing session/catalog of N photos until the browser is populated.
    case open
    /// Select a photo → the first pixels of it on screen (embedded or cached preview).
    case firstPreview = "first-preview"
    /// Select a photo → the full-quality render settled on screen.
    case fullRender = "full-render"
    /// → held: each press → that photo on screen.
    case nextPhoto = "next-photo"
    /// An exposure change → the redraw.
    case exposure
    /// Export N photos as full-size sRGB JPEGs.
    case export

    /// Timed on the viewer from a key, click or script (compared across apps only when
    /// timed on screen); the others are batch operations of seconds, timed by wall clock.
    public var isViewer: Bool { [.firstPreview, .fullRender, .nextPhoto, .exposure].contains(self) }

    /// Whether samples timed by `method` may be compared across apps: viewer operations
    /// only on screen (the in-app clock skips the event path and the compositor);
    /// batch operations by any single wall-clock method.
    public func comparable(method: String) -> Bool {
        isViewer ? method == "screen" : !method.hasPrefix("screen")
    }

    public var title: String {
        switch self {
        case .importFiles: "Import with previews"
        case .open: "Session / catalog open"
        case .firstPreview: "Select → first preview on screen"
        case .fullRender: "Select → full-quality render"
        case .nextPhoto: "Next photo (→ held)"
        case .exposure: "Exposure change → redraw"
        case .export: "Export full-size JPEGs"
        }
    }
}

/// One timed sample. `ms` is nil when the screen never changed before the next
/// stimulus (a press the next one overtook).
public struct BenchRecord: Codable, Equatable, Sendable {
    public enum Mode: String, Codable, Sendable {
        /// Freshly launched app, purged file cache, empty app caches.
        case cold
        /// Same process, repeated: caches hot.
        case warm
    }

    public var app: String
    public var operation: BenchOperation
    public var mode: Mode
    /// Which launch of the app (1-based).
    public var run: Int
    /// Which sample within the run (1-based).
    public var sample: Int
    public var ms: Double?
    /// Photos the sample covers (1 for latencies, N for import/export/open).
    public var items: Int
    /// How it was timed: `in-app`, `screen`, `applescript` or `manual`.
    public var method: String
    /// Import only: the long edge in pixels of the previews the app built.
    public var previewPx: Int?

    public init(app: String, operation: BenchOperation, mode: Mode, run: Int, sample: Int, ms: Double?,
                items: Int = 1, method: String, previewPx: Int? = nil) {
        self.previewPx = previewPx
        self.app = app
        self.operation = operation
        self.mode = mode
        self.run = run
        self.sample = sample
        self.ms = ms
        self.items = items
        self.method = method
    }
}

/// Results files: CSV (one row per sample, for spreadsheets) and JSON.
public enum BenchResults {
    public static let csvHeader = "app,operation,mode,run,sample,ms,items,method,previewPx"
    /// Files written before `previewPx` (8 columns) are still read.
    static let csvHeaderV1 = "app,operation,mode,run,sample,ms,items,method"

    public enum ParseError: Error, Equatable, CustomStringConvertible {
        case header(String)
        case row(line: Int, String)

        public var description: String {
            switch self {
            case .header(let h): "not a results CSV (header \(h.debugDescription))"
            case .row(let line, let text): "line \(line): bad row \(text.debugDescription)"
            }
        }
    }

    public static func csv(_ records: [BenchRecord]) -> String {
        var out = csvHeader + "\n"
        for r in records {
            let ms = r.ms.map { String(format: "%.3f", $0) } ?? ""
            out += [field(r.app), r.operation.rawValue, r.mode.rawValue, "\(r.run)", "\(r.sample)", ms, "\(r.items)",
                    field(r.method), r.previewPx.map(String.init) ?? ""].joined(separator: ",") + "\n"
        }
        return out
    }

    /// Free-text fields never carry the separator.
    static func field(_ s: String) -> String {
        s.replacingOccurrences(of: ",", with: ";").replacingOccurrences(of: "\n", with: " ")
    }

    public static func parseCSV(_ text: String) throws -> [BenchRecord] {
        // Every line kept (blank ones too), so errors name the file's own line numbers.
        var lines = text.components(separatedBy: "\n")
            .map { $0.trimmingCharacters(in: CharacterSet(charactersIn: "\r")) }
        guard let head = lines.first?.trimmingCharacters(in: .whitespaces), head == csvHeader || head == csvHeaderV1 else {
            throw ParseError.header(lines.first ?? "")
        }
        let columns = head == csvHeader ? 9 : 8
        lines.removeFirst()
        return try lines.enumerated().compactMap { i, line in
            if line.trimmingCharacters(in: .whitespaces).isEmpty { return nil }
            let f = line.split(separator: ",", omittingEmptySubsequences: false).map(String.init)
            guard f.count == columns, let op = BenchOperation(rawValue: f[1]), let mode = BenchRecord.Mode(rawValue: f[2]),
                  let run = Int(f[3]), let sample = Int(f[4]), let items = Int(f[6]) else {
                throw ParseError.row(line: i + 2, line)
            }
            let ms: Double?
            if f[5].isEmpty {
                ms = nil
            } else if let v = Double(f[5]) {
                ms = v
            } else {
                throw ParseError.row(line: i + 2, line)
            }
            var previewPx: Int?
            if columns == 9, !f[8].isEmpty {
                guard let px = Int(f[8]) else { throw ParseError.row(line: i + 2, line) }
                previewPx = px
            }
            return BenchRecord(app: f[0], operation: op, mode: mode, run: run, sample: sample, ms: ms, items: items,
                               method: f[7], previewPx: previewPx)
        }
    }

    public static func json(_ records: [BenchRecord]) throws -> Data {
        let e = JSONEncoder()
        e.outputFormatting = [.prettyPrinted, .sortedKeys]
        return try e.encode(records)
    }

    /// Every `*.csv` results file in `folder` (non-recursive), in name order.
    public static func load(folder: URL) throws -> [BenchRecord] {
        let files = try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "csv" }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
        return try files.flatMap { try parseCSV(String(contentsOf: $0, encoding: .utf8)) }
    }

    /// Write `data` atomically (temp file in the same folder, then rename).
    public static func write(_ data: Data, to url: URL) throws {
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try data.write(to: url, options: .atomic)
    }
}
