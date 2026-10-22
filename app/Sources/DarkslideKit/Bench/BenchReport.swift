import Foundation

/// What a results folder was measured on (`meta.json`, written by bench/compare.sh).
public struct BenchMeta: Codable, Equatable, Sendable {
    public struct App: Codable, Equatable, Sendable {
        public var name: String
        public var version: String
        public var build: String?
        /// False: listed in the method but not run on this machine.
        public var ran: Bool
        public var note: String

        public init(name: String, version: String = "", build: String? = nil, ran: Bool = true, note: String = "") {
            self.name = name
            self.version = version
            self.build = build
            self.ran = ran
            self.note = note
        }
    }

    public var date: String
    public var machine: String
    public var os: String
    public var photos: Int
    public var camera: String
    public var apps: [App]
    /// Machine-state checks at the start (power, low power mode, indexing).
    public var preflight: [String]
    /// Launches per app, whether cold runs purged the file cache, the export quality.
    public var runs: Int?
    public var fileCachePurged: Bool?
    public var jpegQuality: Int?

    public init(date: String = "", machine: String = "", os: String = "", photos: Int = 0, camera: String = "",
                apps: [App] = [], preflight: [String] = [], runs: Int? = nil, fileCachePurged: Bool? = nil,
                jpegQuality: Int? = nil) {
        self.runs = runs
        self.fileCachePurged = fileCachePurged
        self.jpegQuality = jpegQuality
        self.date = date
        self.machine = machine
        self.os = os
        self.photos = photos
        self.camera = camera
        self.apps = apps
        self.preflight = preflight
    }
}

/// The markdown report: a summary table (median / p90 / n per app) and a detail table
/// per operation.
public enum BenchReport {
    public static func markdown(_ records: [BenchRecord], meta: BenchMeta) -> String {
        let apps = appOrder(records, meta: meta)
        var out = "# Benchmark results\n\n"
        out += "Method: docs/BENCHMARKS.md. Every number is a median over n samples; p90 is the nearest-rank 90th percentile.\n\n"
        out += "| | |\n|---|---|\n"
        for (k, v) in [("Date", meta.date), ("Machine", meta.machine), ("macOS", meta.os),
                       ("Photos", meta.photos > 0 ? "\(meta.photos) RAW\(meta.camera.isEmpty ? "" : " (\(meta.camera))")" : "")]
            where !v.isEmpty {
            out += "| \(k) | \(v) |\n"
        }
        for a in apps {
            let m = meta.apps.first { $0.name == a }
            let status = ran(a, records: records, meta: meta) ? "run" : "not run"
            let version = m?.version.isEmpty == false ? " \(m!.version)" : ""
            let note = m?.note.isEmpty == false ? ": \(m!.note)" : ""
            out += "| \(a)\(version) | \(status)\(note) |\n"
        }
        if !meta.preflight.isEmpty {
            out += "\nMachine state at the start:\n\n" + meta.preflight.map { "- \($0)\n" }.joined()
        }

        out += "\n## Comparison\n\n"
        out += "Comparable numbers only, never pooled across timing methods: viewer operations timed "
            + "on screen (key, click or script → pixels), batch operations by one wall-clock method per app "
            + "(shown in brackets). median / p90 (n); with n < 10 the second number is the max. Lower is better.\n\n"
        out += "| Operation | Mode | " + apps.joined(separator: " | ") + " |\n"
        out += "|---|---|" + apps.map { _ in "---:|" }.joined() + "\n"
        var anyComparable = false
        for op in BenchOperation.allCases {
            for mode in [BenchRecord.Mode.cold, .warm] {
                let cells = apps.map { a -> String in
                    guard ran(a, records: records, meta: meta) else { return "not run" }
                    guard let method = comparableMethod(records, app: a, op: op, mode: mode),
                          let s = BenchSummary.of(samples(records, app: a, op: op, mode: mode, method: method))
                    else { return "–" }
                    return cell(s) + (op.isViewer ? "" : " [\(method)]")
                }
                guard cells.contains(where: { $0 != "–" && $0 != "not run" }) else { continue }
                anyComparable = true
                out += "| \(op.title) | \(mode.rawValue) | " + cells.joined(separator: " | ") + " |\n"
            }
        }
        if !anyComparable { out += "| (no comparable numbers: viewer operations need screen timing) | | |\n" }

        // Internal timings: the same operation measured inside the app, never compared.
        let internalRows = apps.flatMap { a in
            BenchOperation.allCases.flatMap { op in
                [BenchRecord.Mode.cold, .warm].flatMap { mode in
                    methods(records, app: a, op: op, mode: mode).filter { !op.comparable(method: $0) }.compactMap { m -> String? in
                        guard let s = BenchSummary.of(samples(records, app: a, op: op, mode: mode, method: m)) else { return nil }
                        return "| \(a) | \(op.title) | \(mode.rawValue) | \(m) | \(cell(s)) |"
                    }
                }
            }
        }
        if !internalRows.isEmpty {
            out += "\n## Internal timings (not comparable across apps)\n\n"
            out += "Timed inside the app (e.g. Darkslide-internal: from the call to the frame being ready to draw, "
                + "without the event path or the compositor). Useful to track one app over time only.\n\n"
            out += "| App | Operation | Mode | Method | median / p90 (n) |\n|---|---|---|---|---:|\n"
            out += internalRows.joined(separator: "\n") + "\n"
        }

        out += "\n## Detail\n"
        for op in BenchOperation.allCases {
            let rows = apps.flatMap { a in
                [BenchRecord.Mode.cold, .warm].flatMap { mode in
                    methods(records, app: a, op: op, mode: mode).compactMap { m -> String? in
                        let subset = records.filter { $0.app == a && $0.operation == op && $0.mode == mode && $0.method == m }
                        guard let s = BenchSummary.of(subset.map(\.ms)) else { return nil }
                        let items = subset.map(\.items).max() ?? 1
                        let perPhoto = items > 1 ? duration(s.median / Double(items)) : ""
                        let tag = op.comparable(method: m) ? "" : " (internal)"
                        return "| \(a) | \(mode.rawValue) | \(m)\(tag) | \(s.n) | \(s.missed) | \(duration(s.median)) | "
                            + "\(duration(s.p90)) | \(duration(s.min)) | \(duration(s.max)) | \(perPhoto) |"
                    }
                }
            }
            guard !rows.isEmpty else { continue }
            out += "\n### \(op.title)\n\n"
            out += "| App | Mode | Method | n | Missed | Median | p90 | Min | Max | Per photo |\n"
            out += "|---|---|---|---:|---:|---:|---:|---:|---:|---:|\n"
            out += rows.joined(separator: "\n") + "\n"
        }
        return out
    }

    /// `median / p90 (n)`; with fewer than 10 samples the p90 is the max, and says so.
    /// Missed presses are shown: a median over hits alone flatters an app that misses.
    static func cell(_ s: BenchSummary) -> String {
        let spread = s.n >= 10 ? duration(s.p90) : "max \(duration(s.max))"
        return "\(duration(s.median)) / \(spread) (\(s.n)" + (s.missed > 0 ? ", \(s.missed) missed" : "") + ")"
    }

    static func samples(_ records: [BenchRecord], app: String, op: BenchOperation, mode: BenchRecord.Mode,
                        method: String) -> [Double?] {
        records.filter { $0.app == app && $0.operation == op && $0.mode == mode && $0.method == method }.map(\.ms)
    }

    /// The timing methods an app has samples for, by name.
    static func methods(_ records: [BenchRecord], app: String, op: BenchOperation, mode: BenchRecord.Mode) -> [String] {
        Set(records.filter { $0.app == app && $0.operation == op && $0.mode == mode }.map(\.method)).sorted()
    }

    /// The one method whose samples may stand for `app` in the comparison (the comparable
    /// method with the most samples), or nil.
    static func comparableMethod(_ records: [BenchRecord], app: String, op: BenchOperation,
                                 mode: BenchRecord.Mode) -> String? {
        methods(records, app: app, op: op, mode: mode).filter { op.comparable(method: $0) }
            .max { a, b in
                let na = samples(records, app: app, op: op, mode: mode, method: a).count
                let nb = samples(records, app: app, op: op, mode: mode, method: b).count
                return na != nb ? na < nb : a > b
            }
    }

    /// Apps in `meta` order, then any others found in the records.
    static func appOrder(_ records: [BenchRecord], meta: BenchMeta) -> [String] {
        var out = meta.apps.map(\.name)
        for r in records where !out.contains(r.app) { out.append(r.app) }
        return out
    }

    static func ran(_ app: String, records: [BenchRecord], meta: BenchMeta) -> Bool {
        if let m = meta.apps.first(where: { $0.name == app }), !m.ran { return false }
        return records.contains { $0.app == app }
    }

    /// `850 ms`, `12.5 ms`, `3.42 s`.
    static func duration(_ ms: Double) -> String {
        if ms >= 1000 { return String(format: "%.2f s", ms / 1000) }
        if ms >= 100 { return String(format: "%.0f ms", ms) }
        return String(format: "%.1f ms", ms)
    }
}
