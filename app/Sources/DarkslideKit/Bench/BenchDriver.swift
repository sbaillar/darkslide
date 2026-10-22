import CoreGraphics
import Foundation

/// What the screen driver does to one app (`bench/plans/*.json`).
public struct BenchPlan: Codable, Equatable, Sendable {
    /// Rectangle normalized to the app's main window (0...1, top-left origin).
    public struct UnitRect: Codable, Equatable, Sendable {
        public var x, y, w, h: Double

        public init(x: Double, y: Double, w: Double, h: Double) {
            self.x = x
            self.y = y
            self.w = w
            self.h = h
        }

        /// This rectangle inside `window` (screen points).
        public func within(_ window: CGRect) -> CGRect {
            CGRect(x: window.minX + x * window.width, y: window.minY + y * window.height,
                   width: w * window.width, height: h * window.height).integral
        }
    }

    public enum Stimulus: Codable, Equatable, Sendable {
        /// A key press (virtual key code, e.g. 124 = →) with modifiers (`cmd`, `shift`, `opt`, `ctrl`).
        case key(code: UInt16, modifiers: [String])
        /// A click at a point normalized to the app's main window.
        case click(x: Double, y: Double)
        /// An AppleScript, compiled before the run and executed as the stimulus. `{app}` is
        /// replaced by the app name given to the driver (`--app-name`), never hard-coded.
        case script(source: String)

        /// `{app}` replaced in a script stimulus.
        func resolved(app: String?) -> Stimulus {
            guard case .script(let source) = self, let app else { return self }
            return .script(source: source.replacingOccurrences(of: "{app}", with: app))
        }

        var isScript: Bool { if case .script = self { true } else { false } }
    }

    public struct Step: Codable, Equatable, Sendable {
        /// What each stimulus measures: the first change, the settled screen, or both.
        public var firstChange: BenchOperation?
        public var settled: BenchOperation?
        /// Sent before the step (not timed), e.g. Home to go to the first photo.
        public var prepare: [Stimulus]
        /// Cycled: sample i sends `stimuli[i % count]`.
        public var stimuli: [Stimulus]
        public var count: Int
        /// Seconds from one stimulus to the next.
        public var interval: Double
        /// Match responses to presses in order (held keys): the k-th photo change answers
        /// the k-th press, so an app that lags shows its lag instead of being credited
        /// with the next press's window (`FrameDiff.measureSequence`).
        public var sequence: Bool?

        public init(firstChange: BenchOperation?, settled: BenchOperation? = nil, prepare: [Stimulus] = [],
                    stimuli: [Stimulus], count: Int, interval: Double, sequence: Bool? = nil) {
            self.sequence = sequence
            self.firstChange = firstChange
            self.settled = settled
            self.prepare = prepare
            self.stimuli = stimuli
            self.count = count
            self.interval = interval
        }
    }

    public var app: String
    /// The process name that owns the app's windows (`kCGWindowOwnerName`).
    public var process: String
    /// The image viewer area inside the main window: the recorded region.
    public var viewer: UnitRect
    public var settle: Double
    public var steps: [Step]
    /// An AppleScript returning the name of the app's front document. Plans with script
    /// stimuli must have one: before every script the driver checks that the front
    /// document is the bench copy it was told about, so a script never touches the
    /// user's own catalog or session.
    public var documentCheck: String?

    public init(app: String, process: String, viewer: UnitRect, settle: Double = 0.5, steps: [Step],
                documentCheck: String? = nil) {
        self.documentCheck = documentCheck
        self.app = app
        self.process = process
        self.viewer = viewer
        self.settle = settle
        self.steps = steps
    }

    /// A plan is code (its scripts run): read it before running it. A step with a negative
    /// count or interval is refused here instead of trapping mid-run.
    public static func load(_ url: URL) throws -> BenchPlan {
        let plan = try JSONDecoder().decode(BenchPlan.self, from: Data(contentsOf: url))
        for (n, step) in plan.steps.enumerated() where step.count < 0 || step.interval < 0 {
            throw DecodingError.dataCorrupted(.init(codingPath: [], debugDescription:
                "step \(n + 1) has a negative count or interval"))
        }
        return plan
    }
}

/// The screen: where the app's window is, recording a region, sending stimuli, the clock.
/// `SystemBenchScreen` is the real one (ScreenCaptureKit, CGEvent, AppleScript); tests
/// use a scripted double.
@MainActor
public protocol BenchScreen: AnyObject {
    /// Activate the app (exactly one instance, or the given pid) and return its pid and
    /// its largest on-screen window (screen points, top-left origin).
    func activate(process: String) async throws -> (pid: pid_t, window: CGRect)
    /// The frontmost app's pid: every event is posted only while it is the app under test.
    func frontmostPID() -> pid_t?
    /// Compile the run's scripts up front (so no sample includes compile time).
    func prepare(_ stimuli: [BenchPlan.Stimulus]) throws
    /// Run an AppleScript and return its text result (the front-document check).
    func evaluate(_ script: String) throws -> String
    func startRecording(_ region: CGRect) async throws
    /// Frames since `startRecording`, with their times on `now()`'s clock.
    func stopRecording() async throws -> [LumaFrame]
    func send(_ stimulus: BenchPlan.Stimulus, window: CGRect) throws
    /// Seconds on the clock the frames are stamped with.
    func now() -> Double
    func wait(_ seconds: Double) async
}

/// `Darkslide --bench-drive <plan>`: sends each step's stimuli to the app on screen and
/// times the viewer region's response from a recording (`FrameDiff`).
@MainActor
public struct BenchDriver {
    public enum DriveError: Error, Equatable, CustomStringConvertible {
        case emptyStep(Int)
        case notFrontmost(app: String, frontmost: pid_t?)
        case unguardedScripts
        case appNameNeeded
        case badAppName
        case wrongDocument(expected: String, found: String)

        public var description: String {
            switch self {
            case .emptyStep(let i): "step \(i + 1) has no stimuli"
            case .notFrontmost(let app, let pid):
                "stopped: \(app) is no longer frontmost (frontmost pid \(pid.map(String.init) ?? "none")); "
                    + "no event was sent to another app"
            case .unguardedScripts:
                "the plan sends AppleScripts: it needs a documentCheck and --expect-document (the bench copy's name)"
            case .appNameNeeded: "the plan's scripts name {app}: pass --app-name"
            case .badAppName: "--app-name can't contain a quote or backslash (it is placed inside an AppleScript string)"
            case .wrongDocument(let expected, let found):
                "stopped: the front document is \(found.debugDescription), not the bench copy \(expected.debugDescription)"
            }
        }
    }

    let plan: BenchPlan
    let screen: any BenchScreen
    /// Replaces `{app}` in script stimuli.
    let appName: String?
    /// The bench copy's document name (prefix) every script stimulus requires.
    let expectDocument: String?
    /// Quiet time recorded before the first stimulus of a step (the baseline frame).
    var lead = 0.5

    public init(plan: BenchPlan, screen: any BenchScreen, appName: String? = nil, expectDocument: String? = nil) {
        self.plan = plan
        self.screen = screen
        self.appName = appName
        self.expectDocument = expectDocument
    }

    /// Every event goes out only while the app under test is frontmost, and every script
    /// only while its front document is the bench copy.
    func send(_ stimulus: BenchPlan.Stimulus, pid: pid_t, window: CGRect) throws {
        guard screen.frontmostPID() == pid else { throw DriveError.notFrontmost(app: plan.app, frontmost: screen.frontmostPID()) }
        if stimulus.isScript {
            guard let check = plan.documentCheck, let expected = expectDocument else { throw DriveError.unguardedScripts }
            let found = try screen.evaluate(check.replacingOccurrences(of: "{app}", with: appName ?? ""))
            guard !expected.isEmpty, found.hasPrefix(expected) else {
                throw DriveError.wrongDocument(expected: expected, found: found)
            }
        }
        try screen.send(stimulus.resolved(app: appName), window: window)
    }

    public func run(run: Int, mode: BenchRecord.Mode, log: (String) -> Void = { print($0) }) async throws -> [BenchRecord] {
        let all = plan.steps.flatMap { $0.prepare + $0.stimuli }
        if all.contains(where: \.isScript) {
            guard plan.documentCheck != nil, expectDocument != nil else { throw DriveError.unguardedScripts }
            let names = all.compactMap { if case .script(let s) = $0 { s } else { nil } } + [plan.documentCheck ?? ""]
            if names.contains(where: { $0.contains("{app}") }), appName?.isEmpty ?? true { throw DriveError.appNameNeeded }
            if appName?.contains(where: { $0 == "\"" || $0 == "\\" }) == true { throw DriveError.badAppName }
        }
        try screen.prepare(all.map { $0.resolved(app: appName) })
        let (pid, window) = try await screen.activate(process: plan.process)
        let region = plan.viewer.within(window)
        log("bench-drive: \(plan.app) viewer region \(Int(region.minX)),\(Int(region.minY)),\(Int(region.width)),"
            + "\(Int(region.height)) (check: screencapture -R<that> region.png)")
        let diff = FrameDiff(settle: plan.settle)
        var out: [BenchRecord] = []
        for (n, step) in plan.steps.enumerated() {
            guard !step.stimuli.isEmpty else { throw DriveError.emptyStep(n) }
            for s in step.prepare {
                try send(s, pid: pid, window: window)
                await screen.wait(plan.settle * 2)
            }
            try await screen.startRecording(region)
            await screen.wait(lead)
            var times: [Double] = []
            for i in 0..<max(0, step.count) {
                times.append(screen.now())
                try send(step.stimuli[i % step.stimuli.count], pid: pid, window: window)
                await screen.wait(step.interval)
            }
            await screen.wait(plan.settle)
            let end = screen.now()
            let frames = try await screen.stopRecording()
            var responded = 0
            if step.sequence == true {
                let seq = diff.measureSequence(frames: frames, stimuli: times, end: end)
                responded = seq.latencies.compactMap { $0 }.count
                // An invalid step is kept for the record but never compared (method
                // `screen-invalid`): its matching can't be trusted.
                let method = seq.valid ? "screen" : Self.invalidMethod
                if !seq.valid {
                    log("bench-drive: \(plan.app): \(seq.switches) photo changes for \(step.count) presses: "
                        + "this step is INVALID (a same-photo refine was counted as a new photo); not compared")
                }
                for (i, ms) in seq.latencies.enumerated() {
                    if let op = step.firstChange { out.append(record(op, mode, run, i + 1, ms, method: method)) }
                }
            } else {
                let timings = diff.measure(frames: frames, stimuli: times, end: end)
                responded = timings.filter { $0.firstChange != nil }.count
                for (i, t) in timings.enumerated() {
                    if let op = step.firstChange { out.append(record(op, mode, run, i + 1, t.firstChangeMs)) }
                    if let op = step.settled { out.append(record(op, mode, run, i + 1, t.settledMs)) }
                }
            }
            let ops = [step.firstChange, step.settled].compactMap { $0?.rawValue }.joined(separator: "+")
            log("bench-drive: \(plan.app) \(ops): \(frames.count) frames, \(responded)/\(step.count) responded")
        }
        return out
    }

    /// The method of samples from a step whose press matching failed its check.
    public static let invalidMethod = "screen-invalid"

    func record(_ op: BenchOperation, _ mode: BenchRecord.Mode, _ run: Int, _ sample: Int, _ ms: Double?,
                method: String = "screen") -> BenchRecord {
        BenchRecord(app: plan.app, operation: op, mode: mode, run: run, sample: sample, ms: ms, method: method)
    }
}
