import CoreGraphics
import Foundation
import Testing
@testable import DarkslideKit

/// The benchmark kit (bench/, docs/BENCHMARKS.md): stats, results files, the report,
/// frame diffing, the screen driver (over a scripted screen), the command line, and the
/// in-app runner on the sample RAWs.
@Suite struct BenchStatsTests {
    @Test func medianAndNearestRankP90() throws {
        let odd = try #require(BenchSummary.of([5, 1, 3]))
        #expect(odd.n == 3 && odd.median == 3 && odd.p90 == 5 && odd.min == 1 && odd.max == 5 && odd.missed == 0)
        let even = try #require(BenchSummary.of([4, 1, 3, 2]))
        #expect(even.median == 2.5, "mean of the middle two")
        // 10 values: rank ⌈9⌉ = 9 → the 9th smallest.
        let ten = try #require(BenchSummary.of((1...10).map(Double.init)))
        #expect(ten.p90 == 9 && ten.median == 5.5)
        // 11 values: rank ⌈9.9⌉ = 10.
        #expect(BenchSummary.of((1...11).map(Double.init))?.p90 == 10)
    }

    @Test func missedAndNonFiniteSamplesAreNotTimes() throws {
        let s = try #require(BenchSummary.of([nil, 10, .nan, 20, nil]))
        #expect(s.n == 2 && s.missed == 3 && s.median == 15)
        #expect(BenchSummary.of([]) == nil)
        #expect(BenchSummary.of([nil, nil]) == nil)
    }
}

@Suite struct BenchResultsTests {
    static let records = [
        BenchRecord(app: "Darkslide", operation: .importFiles, mode: .cold, run: 1, sample: 1, ms: 1234.5, items: 100,
                    method: "in-app"),
        BenchRecord(app: "Capture One", operation: .nextPhoto, mode: .warm, run: 2, sample: 7, ms: nil, method: "screen"),
    ]

    @Test func csvRoundTripsIncludingMissedSamples() throws {
        let text = BenchResults.csv(Self.records)
        #expect(text.hasPrefix(BenchResults.csvHeader + "\n"))
        #expect(text.contains("Capture One,next-photo,warm,2,7,,1,screen,"))
        #expect(try BenchResults.parseCSV(text) == Self.records)
    }

    @Test func freeTextNeverBreaksTheColumns() throws {
        let r = BenchRecord(app: "App, Pro", operation: .export, mode: .cold, run: 1, sample: 1, ms: 1, method: "a,b\nc")
        let back = try #require(try BenchResults.parseCSV(BenchResults.csv([r])).first)
        #expect(back.app == "App; Pro" && back.method == "a;b c")
    }

    @Test func previewSizeAndOlderFiles() throws {
        var r = Self.records[0]
        r.previewPx = 1616
        #expect(try BenchResults.parseCSV(BenchResults.csv([r])) == [r])
        let v1 = "app,operation,mode,run,sample,ms,items,method\nLightroom Classic,import,cold,1,1,84210,100,manual\n"
        #expect(try BenchResults.parseCSV(v1).first?.previewPx == nil)
        #expect(throws: BenchResults.ParseError.row(line: 2, "a,open,cold,1,1,2,1,m,big")) {
            try BenchResults.parseCSV("\(BenchResults.csvHeader)\na,open,cold,1,1,2,1,m,big\n")
        }
    }

    @Test func badFilesAreRejectedWithTheLine() {
        #expect(throws: BenchResults.ParseError.header("x,y")) { try BenchResults.parseCSV("x,y\n1,2\n") }
        #expect(throws: BenchResults.ParseError.header("")) { try BenchResults.parseCSV("") }
        let header = BenchResults.csvHeader
        #expect(throws: BenchResults.ParseError.row(line: 2, "a,nope,cold,1,1,2,1,m,")) {
            try BenchResults.parseCSV("\(header)\na,nope,cold,1,1,2,1,m,\n")
        }
        #expect(throws: BenchResults.ParseError.row(line: 3, "a,open,cold,1,1,fast,1,m,")) {
            try BenchResults.parseCSV("\(header)\n\na,open,cold,1,1,fast,1,m,\n")
        }
        #expect(BenchResults.ParseError.row(line: 3, "z").description == "line 3: bad row \"z\"")
        #expect(BenchResults.ParseError.header("q").description.contains("not a results CSV"))
    }

    @Test func loadsEveryCSVInAFolderAndWritesJSON() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("bench-results-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        try BenchResults.write(Data(BenchResults.csv([Self.records[0]]).utf8), to: dir.appendingPathComponent("a.csv"))
        try BenchResults.write(Data(BenchResults.csv([Self.records[1]]).utf8), to: dir.appendingPathComponent("b.csv"))
        try BenchResults.write(try BenchResults.json(Self.records), to: dir.appendingPathComponent("all.json"))
        #expect(try BenchResults.load(folder: dir) == Self.records, "csv files only, in name order")
        let json = try JSONDecoder().decode([BenchRecord].self, from: Data(contentsOf: dir.appendingPathComponent("all.json")))
        #expect(json == Self.records)
    }
}

@Suite struct BenchReportTests {
    static func r(_ app: String, _ op: BenchOperation, _ ms: Double?, mode: BenchRecord.Mode = .cold,
                  items: Int = 1, method: String = "screen") -> BenchRecord {
        BenchRecord(app: app, operation: op, mode: mode, run: 1, sample: 1, ms: ms, items: items, method: method)
    }

    @Test func comparisonNeverPoolsMethodsAndUsesScreenForTheViewer() {
        // Darkslide: in-app (fast, internal) and screen samples for the same operation.
        let records = [0.5, 0.6, 0.7].map { Self.r("Darkslide", .firstPreview, $0, method: "in-app") }
            + [30.0, 40, 50].map { Self.r("Darkslide", .firstPreview, $0) }
            + [Self.r("Capture One", .firstPreview, 120), Self.r("Capture One", .firstPreview, nil)]
            + [Self.r("Darkslide", .export, 50_000, items: 100, method: "in-app"),
               Self.r("Capture One", .export, 61_000, items: 100, method: "applescript")]
        let meta = BenchMeta(date: "2026-10-10", machine: "Mac (M-series)", os: "26.0", photos: 100, camera: "ILCE-9",
                             apps: [.init(name: "Darkslide", version: "0.9"), .init(name: "Capture One", version: "23"),
                                    .init(name: "Lightroom Classic", ran: false, note: "not installed")],
                             preflight: ["power: AC"])
        let md = BenchReport.markdown(records, meta: meta)
        #expect(md.contains("| Photos | 100 RAW (ILCE-9) |") && md.contains("| Darkslide 0.9 | run |"))
        #expect(md.contains("| Lightroom Classic | not run: not installed |") && md.contains("- power: AC"))
        #expect(md.contains("| Operation | Mode | Darkslide | Capture One | Lightroom Classic |"))
        // The viewer row compares screen samples only (40, not a pooled ~15 ms), with misses shown.
        #expect(md.contains("| Select → first preview on screen | cold | 40.0 ms / max 50.0 ms (3) | "
            + "120 ms / max 120 ms (1, 1 missed) | not run |"), "\(md)")
        // Batch rows: one wall-clock method per app, named.
        #expect(md.contains("| Export full-size JPEGs | cold | 50.00 s / max 50.00 s (1) [in-app] | "
            + "61.00 s / max 61.00 s (1) [applescript] | not run |"))
        // In-app viewer timings: their own table, never in the comparison.
        #expect(md.contains("## Internal timings (not comparable across apps)"))
        #expect(md.contains("| Darkslide | Select → first preview on screen | cold | in-app | 0.6 ms / max 0.7 ms (3) |"))
        // Detail: one row per method.
        #expect(md.contains("| Darkslide | cold | in-app (internal) | 3 | 0 | 0.6 ms |"))
        #expect(md.contains("| Darkslide | cold | screen | 3 | 0 | 40.0 ms |"))
        #expect(md.contains("| Darkslide | cold | in-app | 1 | 0 | 50.00 s | 50.00 s | 50.00 s | 50.00 s | 500 ms |"))
        #expect(!md.contains("| Exposure change"), "operations nobody ran are left out")
    }

    @Test func inAppViewerNumbersAloneGiveNoComparison() {
        // A --no-screen run: Darkslide's in-app select timing must not stand against anything.
        let md = BenchReport.markdown([Self.r("Darkslide", .firstPreview, 0.5, method: "in-app")],
                                      meta: BenchMeta(apps: [.init(name: "Darkslide")]))
        #expect(md.contains("no comparable numbers"))
        #expect(!md.contains("| Select → first preview on screen | cold | 0.5"))
        #expect(md.contains("| Darkslide | Select → first preview on screen | cold | in-app |"))
    }

    @Test func manySamplesShowP90AndTheBusiestComparableMethodWins() {
        let records = (1...10).map { Self.r("A", .exposure, Double($0)) }
        #expect(BenchReport.markdown(records, meta: BenchMeta()).contains("| Exposure change → redraw | cold | 5.5 ms / 9.0 ms (10) |"))
        let batch = [Self.r("A", .open, 5, method: "manual"), Self.r("A", .open, 6, method: "applescript"),
                     Self.r("A", .open, 7, method: "applescript")]
        #expect(BenchReport.comparableMethod(batch, app: "A", op: .open, mode: .cold) == "applescript")
        #expect(BenchReport.comparableMethod(batch, app: "A", op: .exposure, mode: .cold) == nil)
        #expect(BenchOperation.exposure.comparable(method: "screen") && !BenchOperation.exposure.comparable(method: "in-app"))
        #expect(BenchOperation.export.comparable(method: "in-app") && !BenchOperation.export.comparable(method: "screen"))
    }

    @Test func appsOnlyInTheRecordsAreListedAndAppsWithoutRecordsAreNotRun() {
        let md = BenchReport.markdown([Self.r("Other", .open, 5, mode: .warm, method: "manual")],
                                      meta: BenchMeta(apps: [.init(name: "Darkslide")]))
        #expect(md.contains("| Darkslide | not run |") && md.contains("| Other | run |"))
        #expect(md.contains("| Session / catalog open | warm | not run | 5.0 ms / max 5.0 ms (1) [manual] |"))
        #expect(!md.contains("| Date |"), "empty meta fields are left out")
    }

    @Test func durations() {
        #expect(BenchReport.duration(12.345) == "12.3 ms")
        #expect(BenchReport.duration(850) == "850 ms")
        #expect(BenchReport.duration(3420) == "3.42 s")
    }
}

@Suite struct FrameDiffTests {
    static func frame(_ t: Double, _ v: UInt8, changed: Int = 16, size: Int = 4) -> LumaFrame {
        var px = [UInt8](repeating: 0, count: size * size)
        for i in 0..<min(changed, px.count) { px[i] = v }
        return LumaFrame(t: t, width: size, height: size, pixels: px)
    }

    @Test func changedFractionCountsPixelsAboveTheThreshold() {
        let d = FrameDiff(pixelThreshold: 16)
        #expect(d.changedFraction(Self.frame(0, 0), Self.frame(0, 10)) == 0, "noise below the threshold")
        #expect(d.changedFraction(Self.frame(0, 0), Self.frame(0, 200)) == 1)
        #expect(d.changedFraction(Self.frame(0, 0), Self.frame(0, 200, changed: 4)) == 0.25)
        #expect(d.changedFraction(Self.frame(0, 0), Self.frame(0, 0, size: 2)) == 1, "a resized region is a change")
    }

    @Test func firstChangeAndSettledPerStimulus() throws {
        let d = FrameDiff(settle: 0.5)
        let frames = [
            Self.frame(0.9, 0),
            Self.frame(1.05, 100), // preview
            Self.frame(1.20, 200), // full render
            Self.frame(1.25, 200, changed: 15), // 1 of 16 pixels: above the 1% floor
            Self.frame(3.04, 50), // second stimulus: changes
            Self.frame(3.10, 60, changed: 0), // noise-level
        ]
        let t = d.measure(frames: frames, stimuli: [3.0, 1.0], end: 4.0)
        #expect(t.count == 2 && t[0].stimulus == 1.0, "stimuli are sorted")
        #expect(abs(try #require(t[0].firstChangeMs) - 50) < 1e-6)
        #expect(abs(try #require(t[0].settledMs) - 250) < 1e-6, "the last change of the first burst (1.25)")
        #expect(abs(try #require(t[1].firstChangeMs) - 40) < 1e-6)
        #expect(abs(try #require(t[1].settledMs) - 100) < 1e-6, "the 3.10 frame changed every pixel back to 0")
    }

    @Test func heldPressesAreMatchedInOrderSoALagStaysALag() {
        // Presses every 100 ms; the app shows each photo 150 ms after its press.
        let presses = [1.0, 1.1, 1.2, 1.3]
        var frames = [Self.frame(0.9, 0)]
        for (i, p) in presses.enumerated() { frames.append(Self.frame(p + 0.15, i % 2 == 0 ? 200 : 0)) }
        let d = FrameDiff()
        let seq = d.measureSequence(frames: frames, stimuli: presses, end: 2)
        #expect(seq.latencies.map { $0.map { $0.rounded() } } == [150, 150, 150, 150], "\(seq)")
        #expect(seq.valid && seq.switches == 4)
        // The windowed measure would credit each press with the previous photo's change.
        #expect(d.measure(frames: frames, stimuli: presses, end: 2)[1].firstChangeMs.map { $0.rounded() } == 50)
        // Two photos for four presses: the last two are missed; a small refine is no new photo.
        let few = [Self.frame(0.9, 0), Self.frame(1.05, 200), Self.frame(1.08, 200, changed: 15), Self.frame(1.25, 0)]
        let fewSeq = d.measureSequence(frames: few, stimuli: presses, end: 2)
        #expect(fewSeq.latencies.map { $0.map { $0.rounded() } } == [50, 150, nil, nil] && fewSeq.valid)
        #expect(d.measureSequence(frames: [], stimuli: [1], end: 2) == .init(latencies: [nil], switches: 0, valid: true))
    }

    @Test func aRefineCountedAsANewPhotoMakesTheStepInvalid() {
        // Each press: the embedded preview, then the developed render of the same photo,
        // different enough to cross the threshold: 2 "photos" per press.
        let presses = [1.0, 1.1, 1.2]
        var frames = [Self.frame(0.9, 0)]
        var v: UInt8 = 0
        for p in presses {
            v = v &+ 60
            frames.append(Self.frame(p + 0.03, v))
            v = v &+ 60
            frames.append(Self.frame(p + 0.06, v))
        }
        let seq = FrameDiff().measureSequence(frames: frames, stimuli: presses, end: 2)
        #expect(seq.switches == 6 && !seq.valid)
        // Frames before the first press (the baseline) never count.
        let early = FrameDiff().measureSequence(frames: [Self.frame(0.1, 0), Self.frame(0.2, 200), Self.frame(1.05, 0)],
                                                stimuli: [1.0], end: 2)
        #expect(early.switches == 1 && early.valid && early.latencies.first??.rounded() == 50)
    }

    @Test func noChangeAndNoSettleAreNil() {
        let d = FrameDiff(settle: 0.5)
        let still = d.measure(frames: [Self.frame(0, 10), Self.frame(1, 10)], stimuli: [0.5], end: 2)
        #expect(still == [FrameDiff.Timing(stimulus: 0.5, firstChange: nil, settled: nil)])
        // Changes every 100 ms until the next stimulus: never quiet for 500 ms.
        let busy = (0..<10).map { Self.frame(0.1 * Double($0), $0 % 2 == 0 ? 0 : 255) }
        let t = d.measure(frames: busy, stimuli: [0.05, 1.0], end: 1.2)
        #expect(t[0].firstChange == 0.1 && t[0].settled == nil)
        #expect(t[1].firstChange == nil, "frames after the last stimulus: none")
        #expect(d.measure(frames: [], stimuli: [1], end: 2).first?.firstChange == nil)
    }
}

/// A screen on a virtual clock: each stimulus makes the region change after
/// `latency` (a preview) and again after `refine` (the full render).
@MainActor
final class ScriptedBenchScreen: BenchScreen {
    var clock = 100.0
    var latency = 0.040
    var refine = 0.200
    /// Each response is a preview and then a refine (false: one frame per response).
    var refines = true
    /// Stimuli with no response (their index).
    var ignore: Set<Int> = []
    var window = CGRect(x: 100, y: 50, width: 1000, height: 800)
    var activated: [String] = []
    var regions: [CGRect] = []
    var sent: [BenchPlan.Stimulus] = []
    private var frames: [LumaFrame] = []
    private var value: UInt8 = 0
    private var stimuli = 0

    /// The pid it reports as the app's, and what is frontmost (another app when they differ).
    var pid: pid_t = 777
    var front: pid_t? = 777
    /// Front document name the check script returns.
    var document = "Bench-r1"
    var compiled: [BenchPlan.Stimulus] = []
    var evaluated: [String] = []

    func activate(process: String) async throws -> (pid: pid_t, window: CGRect) {
        activated.append(process)
        return (pid, window)
    }

    func frontmostPID() -> pid_t? { front }
    func prepare(_ stimuli: [BenchPlan.Stimulus]) throws { compiled = stimuli }
    func evaluate(_ script: String) throws -> String {
        evaluated.append(script)
        return document
    }

    func startRecording(_ region: CGRect) async throws {
        regions.append(region)
        frames = [FrameDiffTests.frame(clock, value)]
    }

    func stopRecording() async throws -> [LumaFrame] { frames }

    func send(_ stimulus: BenchPlan.Stimulus, window: CGRect) throws {
        sent.append(stimulus)
        defer { stimuli += 1 }
        guard !regions.isEmpty, !ignore.contains(stimuli) else { return }
        value = value &+ 60
        frames.append(FrameDiffTests.frame(clock + latency, value))
        guard refines else { return }
        value = value &+ 60
        frames.append(FrameDiffTests.frame(clock + refine, value))
    }

    func now() -> Double { clock }
    func wait(_ seconds: Double) async { clock += seconds }
}

@MainActor @Suite struct BenchDriverTests {
    static let right = BenchPlan.Stimulus.key(code: 124, modifiers: [])

    static func plan(_ steps: [BenchPlan.Step], check: String? = "tell application \"{app}\" to get name of current document")
        -> BenchPlan {
        BenchPlan(app: "Viewer", process: "Viewer App", viewer: .init(x: 0.25, y: 0.1, w: 0.5, h: 0.8), settle: 0.5,
                  steps: steps, documentCheck: check)
    }

    @Test func timesEachStimulusFromTheRecording() async throws {
        let screen = ScriptedBenchScreen()
        screen.ignore = [3] // the 2nd held press (after 1 prepare + 1 select) never shows
        let plan = Self.plan([
            .init(firstChange: .firstPreview, settled: .fullRender, prepare: [.key(code: 115, modifiers: [])],
                  stimuli: [Self.right], count: 1, interval: 1.5),
            .init(firstChange: .nextPhoto, stimuli: [Self.right], count: 3, interval: 0.3),
            .init(firstChange: .exposure, stimuli: [.script(source: "tell application \"{app}\" to up"), .click(x: 0.9, y: 0.5)],
                  count: 2, interval: 1),
        ])
        var lines: [String] = []
        let records = try await BenchDriver(plan: plan, screen: screen, appName: "Viewer 2", expectDocument: "Bench")
            .run(run: 2, mode: .warm) { lines.append($0) }
        #expect(screen.activated == ["Viewer App"])
        #expect(screen.regions.first == CGRect(x: 350, y: 130, width: 500, height: 640), "viewer rect within the window")
        #expect(screen.sent.count == 7, "1 prepare, 1 select, 3 held, 2 exposure")
        #expect(screen.sent.suffix(2) == [.script(source: "tell application \"Viewer 2\" to up"), .click(x: 0.9, y: 0.5)],
                "stimuli cycle, {app} filled in")
        #expect(screen.compiled.contains(.script(source: "tell application \"Viewer 2\" to up")), "compiled before the run")
        #expect(screen.evaluated == ["tell application \"Viewer 2\" to get name of current document"],
                "the front document is checked before the script")
        func ms(_ op: BenchOperation) -> [Double?] {
            records.filter { $0.operation == op }.map { r -> Double? in r.ms.map { $0.rounded() } }
        }
        #expect(ms(.firstPreview) == [40])
        #expect(ms(.fullRender) == [200])
        // Held presses: the ignored one is missed (the screen didn't change in its window).
        #expect(ms(.nextPhoto) == [40, nil, 40])
        #expect(ms(.exposure).count == 2)
        #expect(records.allSatisfy { $0.app == "Viewer" && $0.run == 2 && $0.mode == .warm && $0.method == "screen" })
        #expect(lines.count == 4 && lines[0].contains("350,130,500,640") && lines[2].contains("2/3 responded"))
    }

    @Test func lagIsMatchedPressByPressWhenSequenceIsOn() async throws {
        let screen = ScriptedBenchScreen()
        screen.latency = 0.15 // each photo shows 150 ms after its press
        screen.refines = false
        let plan = Self.plan([.init(firstChange: .nextPhoto, stimuli: [Self.right], count: 4, interval: 0.1, sequence: true)])
        let records = try await BenchDriver(plan: plan, screen: screen).run(run: 1, mode: .cold) { _ in }
        #expect(records.map { $0.ms.map { $0.rounded() } } == [150, 150, 150, 150])
        #expect(records.allSatisfy { $0.method == "screen" })

        // An app that shows a preview and then refines it in place (two changes per press):
        // the step is marked invalid and never compared.
        let refining = ScriptedBenchScreen()
        var lines: [String] = []
        let invalid = try await BenchDriver(plan: plan, screen: refining).run(run: 1, mode: .cold) { lines.append($0) }
        #expect(invalid.count == 4 && invalid.allSatisfy { $0.method == BenchDriver.invalidMethod })
        #expect(lines.contains { $0.contains("INVALID") })
        #expect(!BenchOperation.nextPhoto.comparable(method: BenchDriver.invalidMethod))
        #expect(!BenchOperation.export.comparable(method: BenchDriver.invalidMethod))
        let md = BenchReport.markdown(invalid, meta: BenchMeta())
        #expect(md.contains("no comparable numbers") && md.contains("screen-invalid"))
    }

    @Test func eventsGoOnlyToTheAppUnderTestAndScriptsOnlyToTheBenchCopy() async {
        let key = Self.plan([.init(firstChange: .exposure, stimuli: [Self.right], count: 1, interval: 1)])
        let elsewhere = ScriptedBenchScreen()
        elsewhere.front = 4242 // the user's own app came to the front
        await #expect(throws: BenchDriver.DriveError.notFrontmost(app: "Viewer", frontmost: 4242)) {
            _ = try await BenchDriver(plan: key, screen: elsewhere).run(run: 1, mode: .cold) { _ in }
        }
        #expect(elsewhere.sent.isEmpty, "nothing was posted")

        let script = [BenchPlan.Step(firstChange: .exposure, stimuli: [.script(source: "tell application \"{app}\" to x")],
                                     count: 1, interval: 1)]
        let wrong = ScriptedBenchScreen()
        wrong.document = "My Real Catalog"
        await #expect(throws: BenchDriver.DriveError.wrongDocument(expected: "Bench", found: "My Real Catalog")) {
            _ = try await BenchDriver(plan: Self.plan(script), screen: wrong, appName: "C1", expectDocument: "Bench")
                .run(run: 1, mode: .cold) { _ in }
        }
        #expect(wrong.sent.isEmpty)
        await #expect(throws: BenchDriver.DriveError.unguardedScripts) {
            _ = try await BenchDriver(plan: Self.plan(script), screen: ScriptedBenchScreen(), appName: "C1")
                .run(run: 1, mode: .cold) { _ in }
        }
        await #expect(throws: BenchDriver.DriveError.unguardedScripts) {
            _ = try await BenchDriver(plan: Self.plan(script, check: nil), screen: ScriptedBenchScreen(), appName: "C1",
                                      expectDocument: "Bench").run(run: 1, mode: .cold) { _ in }
        }
        await #expect(throws: BenchDriver.DriveError.appNameNeeded) {
            _ = try await BenchDriver(plan: Self.plan(script), screen: ScriptedBenchScreen(), expectDocument: "Bench")
                .run(run: 1, mode: .cold) { _ in }
        }
        let empty = ScriptedBenchScreen()
        empty.document = ""
        await #expect(throws: BenchDriver.DriveError.wrongDocument(expected: "", found: "")) {
            _ = try await BenchDriver(plan: Self.plan(script), screen: empty, appName: "C1", expectDocument: "")
                .run(run: 1, mode: .cold) { _ in }
        }
        #expect(BenchDriver.DriveError.notFrontmost(app: "A", frontmost: nil).description.contains("no event was sent"))
        #expect(BenchDriver.DriveError.unguardedScripts.description.contains("--expect-document"))
        #expect(BenchDriver.DriveError.appNameNeeded.description.contains("--app-name"))
        #expect(BenchDriver.DriveError.wrongDocument(expected: "B", found: "C").description.contains("not the bench copy"))
    }

    @Test func aStepWithoutStimuliIsAnError() async {
        let plan = Self.plan([.init(firstChange: .exposure, stimuli: [], count: 1, interval: 1)])
        await #expect(throws: BenchDriver.DriveError.emptyStep(0)) {
            _ = try await BenchDriver(plan: plan, screen: ScriptedBenchScreen()).run(run: 1, mode: .cold) { _ in }
        }
        #expect(BenchDriver.DriveError.emptyStep(0).description == "step 1 has no stimuli")
    }

    @Test func plansRoundTripAndTheShippedPlansDecode() throws {
        let plan = Self.plan([.init(firstChange: .nextPhoto, stimuli: [Self.right, .click(x: 1, y: 2), .script(source: "s")],
                                    count: 1, interval: 0.1)])
        let back = try JSONDecoder().decode(BenchPlan.self, from: JSONEncoder().encode(plan))
        #expect(back == plan)
        let dir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("../../../bench/plans")
            .standardizedFileURL
        let files = try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "json" }
        #expect(files.count >= 3)
        for f in files {
            let p = try BenchPlan.load(f)
            let ops = Set(p.steps.flatMap { [$0.firstChange, $0.settled].compactMap { $0 } })
            #expect(ops == [.firstPreview, .fullRender, .nextPhoto, .exposure], "\(f.lastPathComponent)")
            #expect(p.viewer.w > 0 && p.viewer.h > 0 && p.viewer.x + p.viewer.w <= 1 && p.viewer.y + p.viewer.h <= 1)
            #expect(p.steps.first { $0.firstChange == .nextPhoto }?.sequence == true, "held → is matched in order")
            // Scripts never name an app version: always {app}, and always with a document check.
            let scripts = p.steps.flatMap(\.stimuli).compactMap { if case .script(let s) = $0 { s } else { nil } }
            #expect(scripts.allSatisfy { $0.contains("tell application \"{app}\"") }, "\(f.lastPathComponent)")
            if !scripts.isEmpty { #expect(p.documentCheck?.contains("{app}") == true) }
        }
    }
}

@Suite struct LumaSinkTests {
    @Test func bgraRowsToLuma() {
        // 2×2 BGRA with 4 bytes of row padding: white, black / pure green, pure red.
        let bytes: [UInt8] = [255, 255, 255, 255, 0, 0, 0, 255, 9, 9, 9, 9,
                              0, 255, 0, 255, 0, 0, 255, 255, 9, 9, 9, 9]
        let luma = bytes.withUnsafeBytes { LumaSink.luma(bgra: $0, width: 2, height: 2, rowBytes: 12) }
        #expect(luma == [255, 0, 182, 53])
    }
}

@Suite struct BenchCommandTests {
    @Test func parsesTheThreeModes() throws {
        #expect(BenchCommand.wants(["x", "--bench-report", "r"]) && !BenchCommand.wants(["x", "--self-test"]))
        let report = try BenchCommand.parse(["D", "--bench-report", "/tmp/r"])
        #expect(report == .report(results: URL(fileURLWithPath: "/tmp/r"), out: URL(fileURLWithPath: "/tmp/r/report.md")))
        #expect(try BenchCommand.parse(["D", "--bench-report", "/tmp/r", "--out", "/tmp/x.md"])
            == .report(results: URL(fileURLWithPath: "/tmp/r"), out: URL(fileURLWithPath: "/tmp/x.md")))
        #expect(try BenchCommand.parse(["D", "--bench-drive", "/p.json", "--out", "/o.csv", "--run", "3", "--mode", "warm",
                                        "--pid", "4242", "--app-name", "Capture One", "--expect-document", "Bench-r3"])
            == .drive(plan: URL(fileURLWithPath: "/p.json"), out: URL(fileURLWithPath: "/o.csv"), run: 3, mode: .warm,
                      pid: 4242, appName: "Capture One", expectDocument: "Bench-r3"))
        #expect(try BenchCommand.parse(["D", "--bench-drive", "/p.json", "--out", "/o.csv"])
            == .drive(plan: URL(fileURLWithPath: "/p.json"), out: URL(fileURLWithPath: "/o.csv"), run: 1, mode: .cold))
        guard case .bench(let o, let out) = try BenchCommand.parse(
            ["D", "--bench", "/raws", "--work", "/w", "--out", "/o.csv", "--count", "50", "--passes", "3", "--run", "2",
             "--warm-start", "--jumps", "5", "--presses", "7", "--exposure-changes", "4", "--exports", "9",
             "--jpeg-quality", "80"]) else { Issue.record("not bench"); return }
        #expect(o.source.path == "/raws" && o.work.path == "/w" && out.path == "/o.csv")
        #expect(o.count == 50 && o.passes == 3 && o.run == 2 && o.firstMode == .warm)
        #expect(o.jumps == 5 && o.presses == 7 && o.exposureChanges == 4 && o.exports == 9 && o.jpegQuality == 80)
        guard case .bench(let d, _) = try BenchCommand.parse(["D", "--bench", "/raws", "--work", "/w", "--out", "/o"])
        else { return }
        #expect(d.count == 0 && d.passes == 2 && d.firstMode == .cold && d.run == 1 && d.jumps == 20 && d.presses == 100)
    }

    @Test func usageErrors() {
        func error(_ args: [String]) -> String? {
            do {
                _ = try BenchCommand.parse(["D"] + args)
                return nil
            } catch {
                return "\(error)"
            }
        }
        #expect(error(["--bench"]) == "--bench needs a value")
        #expect(error(["--bench", "/r", "--out", "/o"]) == "missing --work")
        #expect(error(["--bench", "/r", "--work", "/w", "--out", "/o", "--count", "lots"]) == "--count needs a whole number, not lots")
        #expect(error(["--bench", "/r", "--work", "/w", "--out", "/o", "--count", "-3"]) == "--count needs a whole number, not -3")
        #expect(error(["--bench-drive", "/p", "--out", "/o", "--mode", "hot"]) == "--mode is cold or warm, not hot")
        #expect(error(["--bench-drive", "/p"]) == "missing --out")
        #expect(error(["--nothing"]) == "no benchmark mode")
        // A bench flag this build doesn't know is an error, never a normal app launch.
        #expect(BenchCommand.wants(["D", "--bench-future", "x"]))
        #expect(error(["--bench-future", "x"]) == "no benchmark mode in --bench-future")
    }

    @MainActor @Test func runWritesReportsAndDriveResults() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("bench-cmd-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        // Drive: a plan on the scripted screen → CSV + JSON.
        let plan = BenchDriverTests.plan([.init(firstChange: .exposure, stimuli: [BenchDriverTests.right], count: 2,
                                                interval: 1)])
        let planURL = dir.appendingPathComponent("plan.json")
        try BenchResults.write(try JSONEncoder().encode(plan), to: planURL)
        let results = dir.appendingPathComponent("results")
        var log: [String] = []
        let status = await BenchCommand.run(.drive(plan: planURL, out: results.appendingPathComponent("viewer.csv"), run: 1,
                                                   mode: .cold),
                                            screen: { _ in ScriptedBenchScreen() }, log: { log.append($0) })
        #expect(status == 0)
        #expect(try BenchResults.load(folder: results).count == 2)
        #expect(FileManager.default.fileExists(atPath: results.appendingPathComponent("viewer.json").path))

        // Report without meta.json, then with it.
        #expect(await BenchCommand.run(.report(results: results, out: dir.appendingPathComponent("r1.md")), log: { log.append($0) }) == 0)
        #expect(try String(contentsOf: dir.appendingPathComponent("r1.md"), encoding: .utf8).contains("| Viewer | run |"))
        try BenchResults.write(try JSONEncoder().encode(BenchMeta(machine: "Test Mac", apps: [.init(name: "Viewer")])),
                               to: results.appendingPathComponent("meta.json"))
        let out = dir.appendingPathComponent("report.md")
        #expect(await BenchCommand.run(.report(results: results, out: out), log: { log.append($0) }) == 0)
        #expect(try String(contentsOf: out, encoding: .utf8).contains("| Machine | Test Mac |"))
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("report.json").path))
        #expect(log.last?.contains("2 samples") == true)

        // Failures are reported, not thrown.
        let missing = dir.appendingPathComponent("nope")
        #expect(await BenchCommand.run(.drive(plan: missing, out: missing, run: 1, mode: .cold), log: { log.append($0) }) == 1)
        let options = DarkslideBench.Options(source: missing, work: dir.appendingPathComponent("w"))
        #expect(await BenchCommand.run(.bench(options, out: missing), log: { log.append($0) }) == 1)
        #expect(log.last?.hasPrefix("bench: ") == true)
    }
}

@MainActor @Suite(.serialized) struct DarkslideBenchTests {
    @Test func rawsAndFreshFolders() throws {
        let src = FileManager.default.temporaryDirectory.appendingPathComponent("bench-src-\(UUID())")
        defer { try? FileManager.default.removeItem(at: src) }
        try FileManager.default.createDirectory(at: src, withIntermediateDirectories: true)
        for n in ["b.ARW", "a.arw", "c.txt", "d.CR3"] { try Data(n.utf8).write(to: src.appendingPathComponent(n)) }
        let all = try DarkslideBench.raws(in: src, count: 0)
        #expect(all.map(\.lastPathComponent) == ["a.arw", "b.ARW", "d.CR3"])
        #expect(try DarkslideBench.raws(in: src, count: 2).map(\.lastPathComponent) == ["a.arw", "b.ARW"])

        let work = src.appendingPathComponent("work")
        let fresh = try DarkslideBench.freshFolder(all, work: work, name: "p1", tag: "r1p1")
        let names = try FileManager.default.contentsOfDirectory(atPath: fresh.path).sorted()
        #expect(names == ["r1p1-0001-a.arw", "r1p1-0002-b.ARW", "r1p1-0003-d.CR3"])
        #expect(try Data(contentsOf: fresh.appendingPathComponent(names[0])) == Data("a.arw".utf8))
        #expect(try DarkslideBench.raws(in: src, count: 0).count == 3, "the source is untouched")
    }

    @Test func errorsSayWhatToDo() async throws {
        #expect(DarkslideBench.BenchError.notPrivate.description.contains("DS_SUPPORT_ROOT"))
        #expect(DarkslideBench.BenchError.noRaws("/x").description == "no RAW files in /x")
        #expect(DarkslideBench.BenchError.timeout("open").description == "timed out: open")
        let empty = FileManager.default.temporaryDirectory.appendingPathComponent("bench-empty-\(UUID())")
        defer { try? FileManager.default.removeItem(at: empty) }
        try FileManager.default.createDirectory(at: empty, withIntermediateDirectories: true)
        let bench = DarkslideBench(options: .init(source: empty, work: empty), defaults: testDefaults())
        await #expect(throws: DarkslideBench.BenchError.noRaws(empty.path)) { _ = try await bench.runPasses { _ in } }
        // Launch safety: never without a scratch support folder and private preferences.
        await #expect(throws: DarkslideBench.BenchError.notPrivate) {
            _ = try await bench.run(environment: [:], privateDefaults: true) { _ in }
        }
        await #expect(throws: DarkslideBench.BenchError.notPrivate) {
            _ = try await bench.run(environment: ["DS_SUPPORT_ROOT": ""], privateDefaults: true) { _ in }
        }
        await #expect(throws: DarkslideBench.BenchError.notPrivate) {
            _ = try await bench.run(environment: ["DS_SUPPORT_ROOT": empty.path], privateDefaults: false) { _ in }
        }
    }

    /// The whole in-app run on the sample RAWs (skipped without them), small counts.
    @Test func runsEveryOperationOnTheSamples() async throws {
        let samples = sampleARWs()
        guard samples.count >= 3 else { return }
        let src = FileManager.default.temporaryDirectory.appendingPathComponent("bench-live-\(UUID())")
        defer { try? FileManager.default.removeItem(at: src) }
        _ = try DarkslideBench.freshFolder(Array(samples.prefix(3)), work: src, name: "raws", tag: "s")
        var o = DarkslideBench.Options(source: src.appendingPathComponent("raws"), work: src.appendingPathComponent("work"),
                                       passes: 2, firstMode: .cold, run: 4)
        o.jumps = 1 // photos 0 → 1, then the held arrow 1 → 2
        o.selectDwell = 0
        o.presses = 2
        o.exposureChanges = 2
        o.exports = 2
        var lines: [String] = []
        let records = try await DarkslideBench(options: o, defaults: testDefaults()).run { lines.append($0) }
        for op in BenchOperation.allCases {
            for mode in [BenchRecord.Mode.cold, .warm] {
                #expect(records.contains { $0.operation == op && $0.mode == mode }, "\(op) \(mode)")
            }
        }
        let timed: [BenchOperation] = [.importFiles, .open, .firstPreview, .fullRender, .exposure, .export]
        #expect(records.filter { timed.contains($0.operation) }.allSatisfy { ($0.ms ?? -1) > 0 }, "\(records)")
        #expect(records.allSatisfy { $0.app == "Darkslide" && $0.run == 4 && $0.method == "in-app" })
        #expect(records.first { $0.operation == .export }?.items == 2)
        #expect(records.first { $0.operation == .importFiles }?.items == 3)
        let px = try #require(records.first { $0.operation == .importFiles }?.previewPx)
        #expect(px > 1000 && px <= 2560, "the viewer preview's real long edge: \(px)")
        #expect(lines.contains { $0.contains("previews \(px) px") })
        #expect(records.filter { $0.operation == .nextPhoto && $0.mode == .cold }.count == 1, "only photo 2 is left")
        #expect(lines.contains { $0.contains("pass 2 (warm)") } && lines.contains { $0.contains("KB each on average") })
        let exported = try FileManager.default.contentsOfDirectory(atPath: o.work.path).filter { $0.hasPrefix("export-") }
        #expect(exported.count == 2)
    }
}
