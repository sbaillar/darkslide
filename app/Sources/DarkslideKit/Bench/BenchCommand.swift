import Foundation

/// The benchmark modes of the `Darkslide` binary (bench/README.md):
///
///     Darkslide --bench <raw-folder> --work <dir> --out <file.csv> [--count N] [--passes P] [--run R] [--warm-start]
///     Darkslide --bench-drive <plan.json> --out <file.csv> [--run R] [--mode cold|warm] [--pid P]
///                             [--app-name <name>] [--expect-document <bench copy name>]
///     Darkslide --bench-report <results-dir> [--out <report.md>]
///     Darkslide --bench-share <results-dir> --photos <folder> [--pack <manifest.json>] [--out <result.json>]
public enum BenchCommand: Equatable, Sendable {
    case bench(DarkslideBench.Options, out: URL)
    /// `pid`: drive that process only (an app the script launched), not any app of the plan's name.
    /// `appName` fills `{app}` in the plan's scripts; `expectDocument` is the bench copy's
    /// document name every script requires at the front.
    case drive(plan: URL, out: URL, run: Int, mode: BenchRecord.Mode, pid: Int32? = nil, appName: String? = nil,
               expectDocument: String? = nil)
    case report(results: URL, out: URL)
    /// The shareable result (`BenchShare`): `photos` is the folder of distinct photos the
    /// run used (the pack's folder, or the staged custom set).
    case share(results: URL, photos: URL, pack: URL?, out: URL)

    public struct UsageError: Error, Equatable, CustomStringConvertible {
        public var description: String
    }

    /// Whether `arguments` ask for a benchmark mode: any `--bench…` argument, known or not,
    /// so a flag this build doesn't know fails in `parse` and never starts the app itself
    /// (with the user's own preferences and library).
    public static func wants(_ arguments: [String]) -> Bool {
        arguments.contains { $0.hasPrefix("--bench") }
    }

    public static func parse(_ arguments: [String]) throws -> BenchCommand {
        func value(_ flag: String) throws -> String? {
            guard let i = arguments.firstIndex(of: flag) else { return nil }
            guard i + 1 < arguments.count, !arguments[i + 1].hasPrefix("--") else {
                throw UsageError(description: "\(flag) needs a value")
            }
            return arguments[i + 1]
        }
        func int(_ flag: String, default d: Int) throws -> Int {
            guard let s = try value(flag) else { return d }
            guard let v = Int(s), v >= 0 else { throw UsageError(description: "\(flag) needs a whole number, not \(s)") }
            return v
        }
        func url(_ s: String) -> URL { URL(fileURLWithPath: s).standardizedFileURL }
        func required(_ flag: String) throws -> URL {
            guard let s = try value(flag) else { throw UsageError(description: "missing \(flag)") }
            return url(s)
        }

        if let results = try value("--bench-share") {
            let dir = url(results)
            return .share(results: dir, photos: try required("--photos"), pack: try value("--pack").map(url),
                          out: try value("--out").map(url) ?? dir.appendingPathComponent("result.json"))
        }
        if let results = try value("--bench-report") {
            let dir = url(results)
            return .report(results: dir, out: try value("--out").map(url) ?? dir.appendingPathComponent("report.md"))
        }
        if let plan = try value("--bench-drive") {
            let mode: BenchRecord.Mode
            switch try value("--mode") ?? "cold" {
            case "cold": mode = .cold
            case "warm": mode = .warm
            case let m: throw UsageError(description: "--mode is cold or warm, not \(m)")
            }
            let pid = try int("--pid", default: 0)
            return .drive(plan: url(plan), out: try required("--out"), run: max(1, try int("--run", default: 1)), mode: mode,
                          pid: pid > 0 ? Int32(pid) : nil, appName: try value("--app-name"),
                          expectDocument: try value("--expect-document"))
        }
        guard let source = try value("--bench") else {
            let flags = arguments.filter { $0.hasPrefix("--bench") }.joined(separator: " ")
            throw UsageError(description: "no benchmark mode" + (flags.isEmpty ? "" : " in \(flags)"))
        }
        var o = DarkslideBench.Options(source: url(source), work: try required("--work"),
                                       count: try int("--count", default: 0), passes: max(1, try int("--passes", default: 2)),
                                       firstMode: arguments.contains("--warm-start") ? .warm : .cold,
                                       run: max(1, try int("--run", default: 1)))
        o.jumps = try int("--jumps", default: o.jumps)
        o.presses = try int("--presses", default: o.presses)
        o.exposureChanges = try int("--exposure-changes", default: o.exposureChanges)
        o.exports = try int("--exports", default: o.exports)
        o.jpegQuality = try int("--jpeg-quality", default: o.jpegQuality)
        return .bench(o, out: try required("--out"))
    }

    /// Run a parsed command; returns the process exit status.
    @MainActor
    public static func run(_ command: BenchCommand,
                           screen: @MainActor (Int32?) -> any BenchScreen = { SystemBenchScreen(pid: $0) },
                           log: (String) -> Void = { print($0) }) async -> Int32 {
        do {
            switch command {
            case .bench(let options, let out):
                let records = try await DarkslideBench(options: options, defaults: AppModel.appDefaults).run(log: log)
                try save(records, to: out)
            case .drive(let plan, let out, let run, let mode, let pid, let appName, let expectDocument):
                let driver = BenchDriver(plan: try BenchPlan.load(plan), screen: screen(pid), appName: appName,
                                         expectDocument: expectDocument)
                let records = try await driver.run(run: run, mode: mode, log: log)
                try save(records, to: out)
            case .report(let results, let out):
                let records = try BenchResults.load(folder: results)
                let meta = try loadMeta(results)
                try BenchResults.write(Data(BenchReport.markdown(records, meta: meta).utf8), to: out)
                try BenchResults.write(try BenchResults.json(records), to: out.deletingPathExtension().appendingPathExtension("json"))
                log("bench: report \(out.path) (\(records.count) samples)")
            case .share(let results, let photos, let pack, let out):
                let share = try await makeShare(results: results, photos: photos, pack: pack, date: Date())
                let problems = BenchShare.privacyProblems(share)
                guard problems.isEmpty else { throw ShareError.privacy(problems) }
                try BenchResults.write(try share.json(), to: out)
                log("bench: shareable result \(out.path) (pack \(share.testPack.id), "
                    + (share.testPack.ranked ? "ranked" : "not ranked") + ")")
            }
            return 0
        } catch {
            log("bench: \(error)")
            return 1
        }
    }

    public enum ShareError: Error, Equatable, CustomStringConvertible {
        case privacy([String])

        public var description: String {
            switch self {
            case .privacy(let p): "not written, it would share private details: " + p.joined(separator: "; ")
            }
        }
    }

    /// `meta.json` of a results folder (empty when there is none).
    static func loadMeta(_ results: URL) throws -> BenchMeta {
        let url = results.appendingPathComponent("meta.json")
        guard FileManager.default.fileExists(atPath: url.path) else { return BenchMeta() }
        return try JSONDecoder().decode(BenchMeta.self, from: Data(contentsOf: url))
    }

    /// The shareable result of a results folder, measured on this Mac with `photos`.
    static func makeShare(results: URL, photos: URL, pack: URL?, date: Date) async throws -> BenchShare {
        let meta = try loadMeta(results)
        let files = try DarkslideBench.raws(in: photos, count: 0)
        guard !files.isEmpty else { throw DarkslideBench.BenchError.noRaws(photos.path) }
        let hash = try BenchShare.contentHash(files)
        let records = try BenchResults.load(folder: results)
        let settings = BenchShare.settings(meta: meta, records: records)
        return BenchShare.make(records: records, meta: meta, machine: .current,
                               cameras: try await BenchShare.cameras(files),
                               testPack: BenchShare.testPack(hash: hash, manifest: try pack.map(BenchPack.load),
                                                             photos: settings.photos),
                               settings: settings, date: date)
    }

    /// Records as CSV at `out` and JSON beside it.
    static func save(_ records: [BenchRecord], to out: URL) throws {
        try BenchResults.write(Data(BenchResults.csv(records).utf8), to: out)
        try BenchResults.write(try BenchResults.json(records), to: out.deletingPathExtension().appendingPathExtension("json"))
    }
}
