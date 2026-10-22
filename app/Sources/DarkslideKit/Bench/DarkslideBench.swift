import CoreGraphics
import Foundation

/// `Darkslide --bench`: the in-app side of the cross-app benchmark (docs/BENCHMARKS.md).
///
/// Drives the real live wiring headlessly (ds-session, embedded previews, the develop
/// renderer, ds-export), the way the keyboard and the sliders do, and times each
/// operation from the call to the moment the viewer state says the photo's pixels are
/// ready to draw. On-screen times (compositor included) come from the screen recorder
/// (`BenchDriver`), the same way as for the other apps.
///
/// It reads `source` only: every pass works on a fresh folder of APFS clones with
/// unique names under `work` (so nothing is cached under the photo's path), and it
/// refuses to run with the user's own preferences or support folder.
@MainActor
public struct DarkslideBench {
    public struct Options: Equatable, Sendable {
        public var source: URL
        public var work: URL
        /// Photos to use (the first `count` RAWs by name; 0 = all).
        public var count: Int
        /// Passes in this process: the first is `firstMode`, the rest warm.
        public var passes: Int
        public var firstMode: BenchRecord.Mode
        /// Launch number, for the records.
        public var run: Int
        /// Viewer size in points (backing scale 2), like a full-screen window.
        public var viewer = CGSize(width: 1600, height: 1000)
        /// Selects: each steps to the next photo after `selectDwell` seconds on the last.
        public var jumps = 20
        public var selectDwell = 2.0
        /// → presses after the selects: needs `jumps + presses + 1` photos to time them all.
        public var presses = 100
        /// Seconds between → presses (a held key's repeat).
        public var pressInterval = 0.1
        public var exposureChanges = 20
        public var exports = 100
        public var jpegQuality = 90
        /// Import is done when every photo has its viewer preview at up to this long edge
        /// (the size the other apps are set to build); the real size is recorded.
        public var previewSize = 2560

        public init(source: URL, work: URL, count: Int = 0, passes: Int = 2, firstMode: BenchRecord.Mode = .cold,
                    run: Int = 1) {
            self.source = source
            self.work = work
            self.count = count
            self.passes = passes
            self.firstMode = firstMode
            self.run = run
        }
    }

    public enum BenchError: Error, Equatable, CustomStringConvertible {
        case notPrivate
        case noRaws(String)
        case timeout(String)

        public var description: String {
            switch self {
            case .notPrivate:
                "--bench needs DS_SUPPORT_ROOT (a scratch folder): it never runs with your own preferences or library"
            case .noRaws(let f): "no RAW files in \(f)"
            case .timeout(let what): "timed out: \(what)"
            }
        }
    }

    public static let app = "Darkslide"
    let options: Options
    let defaults: UserDefaults
    let clock = ContinuousClock()
    /// Longest any single step may take before the run fails.
    var limit: Duration = .seconds(120)

    public init(options: Options, defaults: UserDefaults) {
        self.options = options
        self.defaults = defaults
    }

    /// The RAWs to use: supported RAW extensions, by name, the first `count`.
    public nonisolated static func raws(in folder: URL, count: Int) throws -> [URL] {
        let exts = RustRaw.extensions
        let files = try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)
            .filter { exts.contains($0.pathExtension.lowercased()) }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
        return count > 0 ? Array(files.prefix(count)) : files
    }

    /// Clone `files` into a new folder `<work>/<name>` with unique names (`<tag>-0001-<name>`),
    /// so a repeated pass is new to the app. Clones on APFS: no extra disk, originals untouched.
    public nonisolated static func freshFolder(_ files: [URL], work: URL, name: String, tag: String) throws -> URL {
        let dir = work.appendingPathComponent(name, isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        for (i, f) in files.enumerated() {
            let dest = dir.appendingPathComponent(String(format: "%@-%04d-%@", tag, i + 1, f.lastPathComponent))
            try FileManager.default.copyItem(at: f, to: dest)
        }
        return dir
    }

    /// Refuses unless this process runs on a scratch support folder with private
    /// preferences (`environment` and `privateDefaults` are this process's; tests pass others).
    public func run(environment: [String: String] = ProcessInfo.processInfo.environment,
                    privateDefaults: Bool = AppModel.usesPrivateDefaults,
                    log: (String) -> Void = { print($0) }) async throws -> [BenchRecord] {
        guard privateDefaults, environment["DS_SUPPORT_ROOT"]?.isEmpty == false else { throw BenchError.notPrivate }
        return try await runPasses(log: log)
    }

    /// The passes, without the private-environment check (tests run inside the test
    /// process's own scratch support root).
    func runPasses(log: (String) -> Void) async throws -> [BenchRecord] {
        let files = try Self.raws(in: options.source, count: options.count)
        guard !files.isEmpty else { throw BenchError.noRaws(options.source.path) }
        var out: [BenchRecord] = []
        for pass in 1...max(1, options.passes) {
            let mode: BenchRecord.Mode = pass == 1 ? options.firstMode : .warm
            let folder = try Self.freshFolder(files, work: options.work, name: "run\(options.run)-pass\(pass)",
                                              tag: "r\(options.run)p\(pass)")
            log("bench: pass \(pass) (\(mode.rawValue)), \(files.count) photos in \(folder.path)")
            out += try await self.pass(folder: folder, count: files.count, mode: mode, log: log)
        }
        return out
    }

    func record(_ op: BenchOperation, _ mode: BenchRecord.Mode, _ sample: Int, _ ms: Double?, items: Int = 1) -> BenchRecord {
        BenchRecord(app: Self.app, operation: op, mode: mode, run: options.run, sample: sample, ms: ms, items: items,
                    method: "in-app")
    }

    func makeModel() -> AppModel {
        let settings = AppSettings(defaults: defaults)
        settings.onboardingComplete = true
        settings.autoAdvance = false
        let m = AppModel.make(mode: .live, settings: settings, defaults: defaults)
        m.watchCards = false
        m.viewer.setViewerSize(options.viewer, backingScale: 2)
        return m
    }

    func ms(since t: ContinuousClock.Instant) -> Double { (clock.now - t).inMilliseconds }

    /// Poll (1 ms) until `condition`; throws after `limit`.
    func until(_ what: String, _ condition: () -> Bool) async throws {
        let t = clock.now
        while !condition() {
            if clock.now - t > limit { throw BenchError.timeout(what) }
            try? await Task.sleep(for: .milliseconds(1))
        }
    }

    /// The focused photo itself is on screen (its develop frame or its own preview).
    static func shown(_ m: AppModel) -> Bool {
        guard let f = m.focusedID else { return false }
        return m.viewer.frameID == f || (m.viewer.previewID == f && m.viewer.previewFrame != nil)
    }

    /// The focused photo's settled full-quality develop frame is on screen.
    static func settled(_ m: AppModel) -> Bool {
        guard let f = m.focusedID else { return false }
        return m.viewer.frameID == f && m.viewer.renderedQuality == .preview && m.viewer.isRenderIdle
    }

    func pass(folder: URL, count: Int, mode: BenchRecord.Mode, log: (String) -> Void) async throws -> [BenchRecord] {
        var out: [BenchRecord] = []

        // Import: add the folder in place, until every photo's viewer preview is built at up to
        // `previewSize` (the same "done" as the other apps; the size it really had is recorded).
        let m = makeModel()
        var t = clock.now
        await m.openFolder(folder)
        try await until("import of \(count) photos") { m.visibleIDs.count == count }
        let photos = m.visibleIDs.compactMap { m.photo($0) }
        let images = m.images
        let size = options.previewSize
        let edges = try await withThrowingTaskGroup(of: Int.self) { g in
            for p in photos { g.addTask { let i = try await images.thumbnail(for: p, maxPixelSize: size); return max(i.width, i.height) } }
            var all: [Int] = []
            for try await edge in g { all.append(edge) }
            return all.sorted()
        }
        let previewPx = edges[edges.count / 2]
        var imported = record(.importFiles, mode, 1, ms(since: t), items: count)
        imported.previewPx = previewPx
        out.append(imported)
        log("bench: import \(imported.ms!) ms, previews \(previewPx) px (median long edge)")

        // Select → first preview and → settled full-quality render: the next photo after
        // dwelling on one (as the screen plans do with →; neighbours may be prefetched).
        let ids = m.visibleIDs
        m.focus(ids[0], extend: false)
        try await until("first photo") { Self.settled(m) }
        for i in 0..<min(options.jumps, ids.count - 1) {
            try? await Task.sleep(for: .milliseconds(Int(options.selectDwell * 1000)))
            t = clock.now
            m.focus(ids[i + 1], extend: false)
            try await until("first preview") { Self.shown(m) }
            out.append(record(.firstPreview, mode, i + 1, ms(since: t)))
            try await until("full render") { Self.settled(m) }
            out.append(record(.fullRender, mode, i + 1, ms(since: t)))
        }

        // → held: each press → that photo on screen within the repeat interval, else missed.
        // It goes on from the last selected photo (as the screen plans do), so every press
        // lands on a photo the viewer hasn't shown yet.
        let start = m.focusedID.flatMap { ids.firstIndex(of: $0) } ?? 0
        let presses = min(options.presses, ids.count - 1 - start)
        let interval = Duration.milliseconds(Int(options.pressInterval * 1000))
        for i in 0..<presses {
            t = clock.now
            _ = m.handleKey(KeyInput(special: .right, isRepeat: i > 0), focus: .none, sheetOpen: false, isMainWindow: true)
            var hit: Double?
            while clock.now - t < interval {
                if hit == nil, Self.shown(m) { hit = ms(since: t) }
                try? await Task.sleep(for: .milliseconds(1))
            }
            out.append(record(.nextPhoto, mode, i + 1, hit))
        }
        m.setKeyRepeat(false)

        // Exposure change → the next develop frame on screen.
        try await until("photo after the held arrow") { Self.settled(m) }
        var presented: ContinuousClock.Instant?
        m.viewer.onPresent = { _ in if presented == nil { presented = clock.now } }
        for i in 0..<options.exposureChanges {
            presented = nil
            t = clock.now
            m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = i % 2 == 0 ? 0.3 : 0 }
            m.endEditGesture()
            try await until("exposure redraw") { presented != nil }
            out.append(record(.exposure, mode, i + 1, (presented! - t).inMilliseconds))
            try await until("exposure settled") { Self.settled(m) }
        }
        m.viewer.onPresent = nil
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 0 }
        m.endEditGesture()

        // Export full-size sRGB JPEGs (the defaults for everything else).
        let exported = Array(photos.prefix(options.exports))
        let outDir = options.work.appendingPathComponent("export-run\(options.run)-\(mode.rawValue)-\(UUID().uuidString.prefix(6))")
        let recipe = ExportRecipe(name: "Bench", quality: options.jpegQuality, sizeMode: .original, space: .sRGB,
                                  destination: outDir)
        let job = ExportJob(items: exported.enumerated().compactMap { i, p in
            p.url.map { ExportJobItem(photoID: p.id, url: $0, seq: i + 1) }
        }, recipes: [recipe], defaultFolder: outDir)
        t = clock.now
        let outcome = try await RustExportService().export(job) { (_: Int, _: Int) in }
        let exportMs = ms(since: t)
        if !outcome.failures.isEmpty { log("bench: export failures: \(outcome.failures)") }
        out.append(record(.export, mode, 1, outcome.written.count == exported.count ? exportMs : nil, items: exported.count))
        let kb = outcome.written.isEmpty ? 0 : outcome.written.map(\.bytes).reduce(0, +) / Int64(outcome.written.count) / 1024
        log("bench: export \(outcome.written.count) in \(exportMs) ms, \(kb) KB each on average "
            + "(\(outcome.renderer), \(outcome.jpegEncoder))")

        // Open: a newly launched model opens the session it made (index and previews on disk).
        let m2 = makeModel()
        t = clock.now
        await m2.openFolder(folder)
        try await until("open") { m2.visibleIDs.count == count }
        out.append(record(.open, mode, 1, ms(since: t), items: count))
        return out
    }
}
