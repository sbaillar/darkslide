import Foundation
import Testing
@testable import DarkslideKit

private let next = VoiceAction.key(.move(.right, extend: false))
private let previous = VoiceAction.key(.move(.left, extend: false))
private func rate(_ n: Int) -> VoiceAction { .key(.rate(n)) }

@Suite struct VoiceCommandParserTests {
    private func p(_ s: String) -> [VoiceAction]? { VoiceCommandParser.parse(s) }

    @Test func navigation() {
        for s in ["next", "Next.", "next photo", "next one", "Next photo, please", "go to next", "forward", "skip", "necks"] {
            #expect(p(s) == [next], "\(s)")
        }
        for s in ["previous", "back", "go back", "previous photo", "prev"] { #expect(p(s) == [previous], "\(s)") }
        #expect(p("first") == [.jump(.first)])
        #expect(p("first photo") == [.jump(.first)])
        #expect(p("last") == [.jump(.last)])
        #expect(p("next unrated") == [.jump(.nextUnrated)])
        #expect(p("next pick") == [.jump(.nextPick)], "the longest phrase wins over next + pick")
        #expect(p("next picked") == [.jump(.nextPick)])
    }

    @Test func ratingsWithSynonymsAndHomophones() {
        let cases: [(String, Int)] = [
            ("five", 5), ("5", 5), ("5.", 5), ("Five!", 5), ("five stars", 5), ("5 stars", 5), ("rate it five", 5),
            ("rate five stars", 5), ("fife", 5), ("one", 1), ("one star", 1), ("won", 1), ("1 star", 1),
            ("two", 2), ("to", 2), ("too", 2), ("two stars", 2), ("three", 3), ("tree", 3), ("3 stars", 3),
            ("four", 4), ("for", 4), ("fore", 4), ("rating four", 4), ("zero", 0), ("clear", 0), ("no stars", 0),
            ("clear rating", 0), ("zero stars", 0), ("unrate", 0),
        ]
        for (s, n) in cases { #expect(p(s) == [rate(n)], "\(s)") }
    }

    @Test func flagsAndTags() {
        #expect(p("pick") == [.key(.pick)])
        #expect(p("picked") == [.key(.pick)])
        #expect(p("reject") == [.key(.reject)])
        #expect(p("reject it") == [.key(.reject)])
        #expect(p("unflag") == [.key(.unflag)])
        #expect(p("clear flag") == [.key(.unflag)], "clear flag is unflag, not clear rating")
        #expect(p("un-flag") == [.key(.unflag)])
        for t in ColorTag.allCases where t != .none {
            #expect(p(t.name) == [.key(.tag(t))])
            #expect(p("\(t.name) tag") == [.key(.tag(t))])
            #expect(p("tag \(t.name)") == [.key(.tag(t))])
        }
        #expect(p("read") == [.key(.tag(.red))], "what recognizers hear for red")
    }

    @Test func viewCommands() {
        #expect(p("grid") == [.key(.gridMode)])
        #expect(p("viewer") == [.key(.viewerMode)])
        #expect(p("zoom in") == [.menu(.zoomIn)])
        #expect(p("zoom out") == [.menu(.zoomOut)])
        #expect(p("fit") == [.menu(.zoomFit)])
        #expect(p("zoom to fit") == [.menu(.zoomFit)], "“to” here is not two")
        for s in ["hundred percent", "a hundred percent", "one hundred percent", "100%", "100 percent", "actual size"] {
            #expect(p(s) == [.menu(.zoomActual)], "\(s)")
        }
        #expect(p("before and after") == [.key(.toggleBeforeAfter)])
        #expect(p("before/after") == [.key(.toggleBeforeAfter)])
        #expect(p("full screen") == [.key(.toggleFullScreen)])
        #expect(p("full-screen") == [.key(.toggleFullScreen)])
        #expect(p("exit full screen") == [.setFullScreen(false)])
        #expect(p("hide panels") == [.setPanelsHidden(true)])
        #expect(p("show panels") == [.setPanelsHidden(false)])
    }

    @Test func editingCommands() {
        #expect(p("undo") == [.key(.undo)])
        #expect(p("redo") == [.key(.redo)])
        #expect(p("copy adjustments") == [.menu(.copyAdjustments)])
        #expect(p("paste adjustments") == [.menu(.applyAdjustments)])
        #expect(p("apply adjustments") == [.menu(.applyAdjustments)])
        #expect(p("auto adjust") == [.menu(.autoAdjust)])
        #expect(p("auto-adjust") == [.menu(.autoAdjust)])
    }

    @Test func freeFormEdit() {
        #expect(p("edit warmer and a bit brighter") == [.edit("warmer and a bit brighter")])
        #expect(p("Edit: More contrast.") == [.edit("More contrast")], "the phrase is passed as spoken")
        #expect(p("edit") == nil)
        #expect(p("Edit.") == nil)
        #expect(p("five edit warmer") == nil, "edit only starts an utterance")
    }

    @Test func chaining() {
        #expect(p("five next") == [rate(5), next])
        #expect(p("five, next") == [rate(5), next])
        #expect(p("five and next") == [rate(5), next])
        #expect(p("five stars then next") == [rate(5), next])
        #expect(p("pick next") == [.key(.pick), next])
        #expect(p("red five next") == [.key(.tag(.red)), rate(5), next])
        #expect(p("next next") == [next, next])
        #expect(p("five next four next") == [rate(5), next, rate(4), next])
        #expect(p("next one") == [next], "“one” after next is the photo, not a rating")
    }

    @Test func rejectsEverydaySpeech() {
        for s in ["", "   ", "the", "please", "photo", "I'll be back", "what's next for dinner", "go to the store",
                  "five o'clock", "red wine", "two four", "one two three", "pick up the kids", "next week",
                  "that's a five star restaurant", "is it good", "stars", "rate", "zoom", "edited"] {
            #expect(p(s) == nil, "\(s)")
        }
    }

    @Test func labels() {
        #expect(rate(5).label == "★★★★★")
        #expect(rate(1).label == "★")
        #expect(rate(0).label == "No stars")
        #expect(next.label == "Next")
        #expect(previous.label == "Previous")
        #expect(VoiceAction.key(.tag(.red)).label == "Red tag")
        #expect(VoiceAction.key(.pick).label == "Pick")
        #expect(VoiceAction.key(.reject).label == "Reject")
        #expect(VoiceAction.key(.unflag).label == "Unflag")
        #expect(VoiceAction.key(.gridMode).label == "Grid")
        #expect(VoiceAction.key(.viewerMode).label == "Viewer")
        #expect(VoiceAction.key(.toggleBeforeAfter).label == "Before / after")
        #expect(VoiceAction.key(.toggleFullScreen).label == "Full screen")
        #expect(VoiceAction.key(.togglePanels).label == "Panels")
        #expect(VoiceAction.key(.undo).label == "Undo")
        #expect(VoiceAction.key(.redo).label == "Redo")
        #expect(VoiceAction.key(.selectAll).label == "selectAll")
        #expect(VoiceAction.menu(.zoomIn).label == "Zoom in")
        #expect(VoiceAction.menu(.zoomOut).label == "Zoom out")
        #expect(VoiceAction.menu(.zoomFit).label == "Fit")
        #expect(VoiceAction.menu(.zoomActual).label == "100%")
        #expect(VoiceAction.menu(.copyAdjustments).label == "Copy adjustments")
        #expect(VoiceAction.menu(.applyAdjustments).label == "Apply adjustments")
        #expect(VoiceAction.menu(.autoAdjust).label == "Auto adjust")
        #expect(VoiceAction.menu(.survey).label == "survey")
        #expect(VoiceAction.jump(.first).label == "First")
        #expect(VoiceAction.jump(.last).label == "Last")
        #expect(VoiceAction.jump(.nextUnrated).label == "Next unrated")
        #expect(VoiceAction.jump(.nextPick).label == "Next pick")
        #expect(VoiceAction.setPanelsHidden(true).label == "Hide panels")
        #expect(VoiceAction.setPanelsHidden(false).label == "Show panels")
        #expect(VoiceAction.setFullScreen(true).label == "Full screen")
        #expect(VoiceAction.setFullScreen(false).label == "Exit full screen")
        #expect(VoiceAction.edit("warmer").label == "Edit: “warmer”, press Return")
    }

    @Test func vocabularyBiasesTowardCommands() {
        let v = VoiceCommandParser.vocabulary
        for w in ["next", "previous", "pick", "reject", "five", "stars", "red", "zoom in", "edit", "hundred percent"] {
            #expect(v.contains(w), "\(w)")
        }
        #expect(!v.contains { $0.contains(where: \.isNumber) }, "digits are the recognizer's own business")
        #expect(v == v.sorted())
    }

    /// What a partial may fire before the final.
    @Test func settledPartials() {
        func settled(_ s: String) -> Bool? { VoiceCommandParser.analyze(s)?.settled }
        #expect(settled("five stars") == true)
        #expect(settled("pick") == true)
        #expect(settled("zoom in") == true)
        #expect(settled("five stars next one") == true)
        #expect(settled("five") == false, "may still become “five stars” or “five o'clock”")
        #expect(settled("next") == false, "may still become “next pick”")
        #expect(settled("clear") == false, "may still become “clear flag”")
        #expect(settled("for") == false, "homophone: “for the record”")
        #expect(settled("to") == false)
        #expect(settled("back") == false, "“back in the day”")
        #expect(settled("read") == false)
        #expect(settled("view") == false)
        #expect(settled("pick next") == false, "ends in next")
        #expect(settled("edit warmer") == false)
        #expect(VoiceCommandParser.analyze("five stars, then pick")?.words == 3, "fillers don't count")
    }

    /// Parsing is far below the ~300 ms budget from end of speech to action.
    @Test func parserLatency() {
        let utterances = ["five next", "next photo please", "what's next for dinner", "rate it three stars then next",
                          "edit warmer and a bit brighter", "one hundred percent", "red five next", "go to the store"]
        let n = 2_000
        let clock = ContinuousClock()
        let elapsed = clock.measure {
            for i in 0..<n { _ = VoiceCommandParser.parse(utterances[i % utterances.count]) }
        }
        let perParse = elapsed / n
        print("voice parser: \(perParse) per utterance (\(n) parses)")
        #expect(perParse < .milliseconds(1))
    }
}

@Suite struct VoiceUtteranceTrackerTests {
    private func t(_ s: String, final: Bool = false, confidence: Double? = 0.9) -> VoiceTranscript {
        VoiceTranscript(s, isFinal: final, confidence: confidence)
    }

    /// Partials then a final; returns what fired, in order (ticks well after each partial).
    private func feed(_ partials: [String], final: String?, mode: VoiceUtteranceTracker.Mode = .continuous) -> [VoiceAction] {
        var tr = VoiceUtteranceTracker(mode: mode, settle: 0.2)
        var out: [VoiceAction] = []
        var now = 0.0
        for p in partials {
            out += tr.receive(t(p), at: now)
            now += 1
            out += tr.tick(at: now)
        }
        if let final { out += tr.receive(t(final, final: true), at: now) }
        return out
    }

    @Test func settledMultiWordPartialFiresOnceThenFinalAddsNothing() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        #expect(tr.receive(t("five stars"), at: 0).isEmpty)
        #expect(tr.deadline == 0.2)
        #expect(tr.tick(at: 0.1).isEmpty, "not settled yet")
        #expect(tr.tick(at: 0.2) == [rate(5)])
        #expect(tr.tick(at: 0.5).isEmpty, "fires once")
        #expect(tr.receive(t("five stars", final: true), at: 0.6).isEmpty, "the final doesn't fire it again")
        #expect(tr.fired.isEmpty, "the utterance is over")
    }

    @Test func continuousSingleWordsWaitForTheFinal() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        _ = tr.receive(t("pick"), at: 0)
        #expect(tr.deadline == nil)
        #expect(tr.tick(at: 5).isEmpty)
        #expect(tr.receive(t("pick", final: true), at: 6) == [.key(.pick)])
    }

    @Test func pushToTalkFiresSettledSingleWordsEarly() {
        var tr = VoiceUtteranceTracker(mode: .pushToTalk, settle: 0.2)
        _ = tr.receive(t("pick"), at: 0)
        #expect(tr.tick(at: 0.2) == [.key(.pick)])
        _ = tr.receive(t("pick next"), at: 0.3)
        #expect(tr.deadline == nil, "next may become next pick: only release or the final")
        // review-g H4: a release on a word that leads into more waits for the final.
        #expect(tr.flush().isEmpty, "release on “next”: the final decides")
        #expect(tr.receive(t("pick next", final: true), at: 0.5) == [next])
    }

    /// The reviewer's everyday phrases: nothing fires, or only what the final means.
    @Test func everydaySpeechNeverFiresTheWrongThing() {
        #expect(feed(["for", "for the", "for the record"], final: "for the record").isEmpty)
        #expect(feed(["to", "to be", "to be honest"], final: "to be honest").isEmpty)
        #expect(feed(["back", "back in", "back in the day"], final: "back in the day").isEmpty)
        #expect(feed(["five", "five o'clock"], final: "five o'clock works").isEmpty)
        #expect(feed(["clear", "clear flag"], final: "clear flag") == [.key(.unflag)])
        #expect(feed(["one", "one hundred", "one hundred percent"], final: "one hundred percent") == [.menu(.zoomActual)])
        #expect(feed(["next", "next pick"], final: "next pick") == [.jump(.nextPick)])
        for mode in [VoiceUtteranceTracker.Mode.continuous, .pushToTalk] {
            #expect(feed(["for", "for the record"], final: nil, mode: mode).isEmpty, "\(mode)")
            #expect(feed(["clear"], final: "clear flag", mode: mode) == [.key(.unflag)], "\(mode)")
            #expect(feed(["next"], final: "next pick", mode: mode) == [.jump(.nextPick)], "\(mode)")
            #expect(feed(["one"], final: "one hundred percent", mode: mode) == [.menu(.zoomActual)], "\(mode)")
        }
    }

    @Test func refiningPartialsFireOnlyWhatIsNew() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        _ = tr.receive(t("five stars"), at: 0)
        #expect(tr.tick(at: 0.25) == [rate(5)])
        _ = tr.receive(t("five stars pick"), at: 0.3)
        #expect(tr.tick(at: 0.5) == [.key(.pick)])
        #expect(tr.receive(t("five stars pick next", final: true), at: 0.7) == [next])
    }

    @Test func identicalPartialsDontPostponeFiring() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        _ = tr.receive(t("zoom in"), at: 0)
        _ = tr.receive(t("zoom in"), at: 0.1)
        _ = tr.receive(t("Zoom in."), at: 0.15)
        #expect(tr.tick(at: 0.2) == [.menu(.zoomIn)])
    }

    @Test func changingPartialRestartsTheWait() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        _ = tr.receive(t("five stars"), at: 0)
        _ = tr.receive(t("5 stars"), at: 0.1)
        #expect(tr.deadline == 0.2, "same command: the wait goes on")
        _ = tr.receive(t("five stars pick"), at: 0.15)
        #expect(tr.tick(at: 0.2).isEmpty, "a new command: wait again")
        #expect(tr.tick(at: 0.35) == [rate(5), .key(.pick)])
    }

    @Test func partialWithoutConfidenceWaitsLonger() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        _ = tr.receive(t("zoom in", confidence: nil), at: 0)
        #expect(tr.deadline == 0.4, "unknown confidence isn't certainty")
        #expect(tr.tick(at: 0.3).isEmpty)
        #expect(tr.tick(at: 0.4) == [.menu(.zoomIn)])
    }

    @Test func finalFiresWhatPartialsDidNot() {
        var tr = VoiceUtteranceTracker()
        _ = tr.receive(t("five"), at: 0)
        #expect(tr.receive(t("five next", final: true), at: 0.1) == [rate(5), next])
        #expect(tr.pending == nil && tr.takeContradiction() == nil)
    }

    @Test func sameCommandInTwoUtterancesFiresTwice() {
        var tr = VoiceUtteranceTracker()
        #expect(tr.receive(t("next", final: true), at: 0) == [next])
        #expect(tr.receive(t("next", final: true), at: 1) == [next])
    }

    @Test func lowConfidenceNeverFires() {
        var tr = VoiceUtteranceTracker(settle: 0.2, minConfidence: 0.5)
        _ = tr.receive(t("five stars", confidence: 0.2), at: 0)
        #expect(tr.pending == nil)
        #expect(tr.receive(t("five", final: true, confidence: 0.3), at: 0.5).isEmpty)
        #expect(tr.receive(t("five", final: true, confidence: 0.9), at: 1) == [rate(5)])
        #expect(tr.receive(t("pick", final: true, confidence: nil), at: 2) == [.key(.pick)], "a final without confidence counts")
    }

    @Test func editFiresOnlyFromTheFinal() {
        var tr = VoiceUtteranceTracker(mode: .pushToTalk, settle: 0.2)
        _ = tr.receive(t("edit warmer"), at: 0)
        #expect(tr.pending == nil)
        #expect(tr.tick(at: 1).isEmpty, "a pause mid-instruction sends nothing")
        #expect(tr.flush().isEmpty)
        #expect(tr.receive(t("edit warmer and a bit brighter", final: true), at: 1.5)
            == [.edit("warmer and a bit brighter")])
    }

    @Test func finalContradictingWhatFiredSaysSo() {
        var tr = VoiceUtteranceTracker(settle: 0.2)
        _ = tr.receive(t("five stars"), at: 0)
        #expect(tr.tick(at: 0.2) == [rate(5)])
        #expect(tr.receive(t("pick", final: true), at: 0.5).isEmpty, "never a different command for the same words")
        #expect(tr.takeContradiction() == "pick")
        #expect(tr.takeContradiction() == nil, "said once")
        _ = tr.receive(t("zoom in"), at: 1)
        #expect(tr.tick(at: 1.3) == [.menu(.zoomIn)])
        #expect(tr.receive(t("zoom in on her face", final: true), at: 1.5).isEmpty)
        #expect(tr.takeContradiction() == "zoom in on her face", "it was talk after all")
        #expect(tr.receive(t("what a day", final: true), at: 2).isEmpty)
        #expect(tr.takeContradiction() == nil, "nothing fired, nothing to take back")
    }

    @Test func flushFiresPendingNowAndTheFinalOnlyTheRest() {
        var tr = VoiceUtteranceTracker(mode: .pushToTalk, settle: 0.2)
        #expect(tr.flush().isEmpty)
        _ = tr.receive(t("five"), at: 0)
        #expect(tr.deadline == nil, "five may still grow")
        // review-g2 H4': an unsettled partial waits for the final ("five" may be "five stars").
        #expect(tr.flush().isEmpty)
        #expect(tr.receive(t("five next", final: true), at: 0.3) == [rate(5), next])
        _ = tr.receive(t("five stars"), at: 0.5)
        #expect(tr.flush() == [rate(5)], "push-to-talk released: speech is over")
        #expect(tr.receive(t("five stars next", final: true), at: 0.6) == [next])
        _ = tr.receive(t("pick"), at: 1)
        tr.reset()
        #expect(tr.fired.isEmpty && tr.pending == nil)
    }
}

// MARK: - Controller

/// Engine double with the real engine's session rules: `finish`/`stop` act only on
/// the current session; tests push events through `send`.
final class FakeVoiceEngine: VoiceEngine, @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: AsyncStream<VoiceEngineEvent>.Continuation?
    private var current: Int?
    private var latest = 0
    private var _starts = 0, _stops: [Int] = [], _finishes: [Int] = []
    private var _vocabulary: [String] = []
    var startError: Error?
    /// Holds `start` until `release()` (to turn voice off while it starts).
    private var gate: CheckedContinuation<Void, Never>?
    var gated = false

    var starts: Int { lock.withLock { _starts } }
    /// Sessions actually stopped / finished (calls for other sessions are ignored).
    var stops: [Int] { lock.withLock { _stops } }
    var finishes: [Int] { lock.withLock { _finishes } }
    var session: Int? { lock.withLock { current } }
    var vocabulary: [String] { lock.withLock { _vocabulary } }
    var isWaiting: Bool { lock.withLock { gate != nil } }

    func start(vocabulary: [String]) async throws -> VoiceSession {
        let id = lock.withLock { () -> Int in
            _starts += 1
            _vocabulary = vocabulary
            latest += 1
            current = latest
            return latest
        }
        if lock.withLock({ gated }) { await withCheckedContinuation { c in lock.withLock { gate = c } } }
        if let startError { throw startError }
        guard lock.withLock({ current == id }) else { throw VoiceEngineError.superseded }
        let (stream, c) = AsyncStream<VoiceEngineEvent>.makeStream()
        lock.withLock { continuation = c }
        return VoiceSession(id: id, events: stream)
    }

    func release() {
        let g = lock.withLock { () -> CheckedContinuation<Void, Never>? in
            defer { gate = nil; gated = false }
            return gate
        }
        g?.resume()
    }

    func send(_ e: VoiceEngineEvent) { _ = lock.withLock { continuation }?.yield(e) }

    /// The input changed: the session ends by itself.
    func endByItself() {
        let c = lock.withLock { () -> AsyncStream<VoiceEngineEvent>.Continuation? in
            defer { continuation = nil; current = nil }
            return continuation
        }
        c?.finish()
    }

    private func end(_ session: Int, into log: ReferenceWritableKeyPath<FakeVoiceEngine, [Int]>) {
        let c = lock.withLock { () -> AsyncStream<VoiceEngineEvent>.Continuation? in
            guard current == session else { return nil }
            self[keyPath: log].append(session)
            defer { continuation = nil; current = nil }
            return continuation
        }
        c?.finish()
    }

    /// `finish` keeps the session's stream open until `deliverLateFinal` (a slow recognizer).
    var holdsFinishing = false
    private var finishing: [Int: AsyncStream<VoiceEngineEvent>.Continuation] = [:]

    /// The finished session's final arrives late, then its stream ends.
    func deliverLateFinal(_ t: VoiceTranscript, session: Int) {
        let c = lock.withLock { finishing.removeValue(forKey: session) }
        c?.yield(.transcript(t))
        c?.finish()
    }

    func finish(_ session: Int) async {
        let held = lock.withLock { () -> Bool in
            guard holdsFinishing, current == session else { return false }
            _finishes.append(session)
            finishing[session] = continuation
            continuation = nil
            current = nil
            return true
        }
        if !held { end(session, into: \._finishes) }
    }
    func stop(_ session: Int) async { end(session, into: \._stops) }
}

final class FakeVoicePermissions: VoicePermissions, @unchecked Sendable {
    private let lock = NSLock()
    private var mic: VoicePermission, speechStatus: VoicePermission
    private var grantOnRequest: Bool
    private(set) var requests: [String] = []

    init(mic: VoicePermission = .granted, speech: VoicePermission = .granted, grantOnRequest: Bool = true) {
        self.mic = mic
        speechStatus = speech
        self.grantOnRequest = grantOnRequest
    }

    func microphone() -> VoicePermission { lock.withLock { mic } }
    func speech() -> VoicePermission { lock.withLock { speechStatus } }
    func requestMicrophone() async -> Bool {
        lock.withLock {
            requests.append("mic")
            mic = grantOnRequest ? .granted : .denied
            return grantOnRequest
        }
    }
    func requestSpeech() async -> Bool {
        lock.withLock {
            requests.append("speech")
            speechStatus = grantOnRequest ? .granted : .denied
            return grantOnRequest
        }
    }
}

@MainActor
@Suite struct VoiceControllerTests {
    final class Clock {
        var t = 0.0
    }

    final class Ran {
        var all: [[VoiceAction]] = []
    }

    private func make(_ engine: FakeVoiceEngine = FakeVoiceEngine(), permissions: FakeVoicePermissions = FakeVoicePermissions())
        -> (VoiceController, FakeVoiceEngine, Ran, Clock) {
        let v = VoiceController(engine: engine, permissions: permissions)
        let ran = Ran()
        let clock = Clock()
        v.now = { clock.t }
        v.sleep = { _ in }
        v.echoDuration = 0
        v.perform = { actions, _ in
            ran.all.append(actions)
            return actions.map(\.label).joined(separator: " · ")
        }
        return (v, engine, ran, clock)
    }

    /// Lets queued main-actor work (the listen loop) run.
    private func settle() async { for _ in 0..<20 { await Task.yield() } }

    @Test func defaultsNeverListen() async {
        let v = VoiceController()
        await v.turnOn()
        #expect(v.problem == .microphoneDenied && !v.isOn, "an unwired controller never prompts or listens")
        var chain = VoiceChain()
        #expect(v.perform([next], &chain) == nil)
        let engine = UnavailableVoiceEngine()
        await #expect(throws: VoiceEngineError.unavailable("Voice commands aren't available in this build.")) {
            _ = try await engine.start(vocabulary: [])
        }
        await engine.finish(1)
        await engine.stop(1)
        let none = NoVoicePermissions()
        let mic = await none.requestMicrophone(), speech = await none.requestSpeech()
        #expect(!mic && !speech && none.speech() == .denied && none.microphone() == .denied)
    }

    @Test func turningOnAsksOnceThenListens() async {
        let perms = FakeVoicePermissions(mic: .notDetermined, speech: .notDetermined)
        let (v, engine, _, _) = make(permissions: perms)
        #expect(v.status == .off)
        await v.turnOn()
        #expect(perms.requests == ["mic", "speech"])
        #expect(await waitUntil { v.status == .listening })
        #expect(engine.starts == 1)
        #expect(engine.vocabulary.contains("next"))
        await v.turnOn()
        #expect(engine.starts == 1, "already on")
        v.turnOff()
        await v.turnOn()
        #expect(perms.requests == ["mic", "speech"], "asked only the first time")
        v.turnOff()
    }

    @Test func toggleFlipsTheSwitchAtOnce() async {
        let (v, engine, _, _) = make()
        v.toggle()
        #expect(v.isOn && v.status == .starting, "no waiting for a task to flip it")
        v.toggle()
        #expect(!v.isOn && v.status == .off)
        await settle()
        #expect(!v.isOn && engine.starts == 0, "the stale permission answer doesn't turn it back on")
        v.toggle()
        #expect(await waitUntil { v.status == .listening })
        v.toggle()
    }

    @Test func deniedMicrophoneSaysSoWithSettings() async {
        let (v, engine, _, _) = make(permissions: FakeVoicePermissions(mic: .notDetermined, grantOnRequest: false))
        await v.turnOn()
        #expect(!v.isOn && v.status == .off)
        #expect(v.problem == .microphoneDenied)
        #expect(v.problem?.settingsURL?.absoluteString.contains("Privacy_Microphone") == true)
        #expect(engine.starts == 0)
        v.dismissProblem()
        #expect(v.problem == nil)
    }

    @Test func deniedSpeechSaysSoWithSettings() async {
        let (v, _, _, _) = make(permissions: FakeVoicePermissions(speech: .denied))
        await v.turnOn()
        #expect(v.problem == .speechDenied && !v.isOn)
        #expect(v.problem?.settingsURL?.absoluteString.contains("Privacy_SpeechRecognition") == true)
    }

    @Test func unavailableRecognitionTurnsVoiceOff() async {
        let engine = FakeVoiceEngine()
        engine.startError = VoiceEngineError.unavailable("No on-device English")
        let (v, _, _, _) = make(engine)
        await v.turnOn()
        #expect(await waitUntil { v.problem != nil })
        #expect(v.problem == .unavailable("No on-device English"))
        #expect(v.problem?.settingsURL == nil)
        #expect(!v.isOn && v.status == .off)
    }

    @Test func otherStartErrorsAreReported() async {
        let engine = FakeVoiceEngine()
        engine.startError = CocoaError(.fileNoSuchFile)
        let (v, _, _, _) = make(engine)
        await v.turnOn()
        #expect(await waitUntil { v.problem != nil })
        #expect(v.problem == .unavailable(CocoaError(.fileNoSuchFile).localizedDescription))
    }

    @Test func engineFailureTurnsVoiceOff() async {
        let (v, engine, _, _) = make()
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        let id = engine.session!
        engine.send(.failed("The microphone went away"))
        #expect(await waitUntil { !v.isOn })
        #expect(v.problem == .unavailable("The microphone went away"))
        #expect(await waitUntil { engine.stops == [id] })
    }

    @Test func turningOffReleasesTheMicrophone() async {
        let (v, engine, _, _) = make()
        v.toggle()
        #expect(await waitUntil { v.status == .listening })
        let id = engine.session!
        v.toggle()
        #expect(v.status == .off)
        #expect(await waitUntil { engine.stops == [id] && engine.session == nil })
    }

    @Test func turningOffWhileStartingStopsThatSession() async {
        let engine = FakeVoiceEngine()
        engine.gated = true
        let (v, _, ran, _) = make(engine)
        await v.turnOn()
        #expect(v.status == .starting)
        #expect(await waitUntil { engine.isWaiting })
        v.turnOff()
        engine.release()
        #expect(await waitUntil { engine.stops == [1] }, "the late session is stopped when it arrives")
        engine.send(.transcript(VoiceTranscript("next", isFinal: true)))
        await settle()
        #expect(ran.all.isEmpty)
    }

    /// MED-1: an older run's late stop never kills the newer session.
    @Test func staleStopNeverStopsANewerSession() async {
        let engine = FakeVoiceEngine()
        engine.gated = true
        let (v, _, ran, _) = make(engine)
        await v.turnOn()
        #expect(await waitUntil { engine.isWaiting })
        v.turnOff()
        await v.turnOn() // a second run starts while the first is still starting
        #expect(await waitUntil { engine.starts == 2 })
        engine.release()
        #expect(await waitUntil { v.status == .listening })
        #expect(engine.session == 2)
        #expect(engine.stops.isEmpty, "session 1 was superseded; nothing stopped session 2")
        engine.send(.transcript(VoiceTranscript("pick", isFinal: true)))
        #expect(await waitUntil { ran.all == [[.key(.pick)]] })
        v.turnOff()
    }

    @Test func runEndingByItselfStartsANewOne() async {
        let (v, engine, ran, clock) = make()
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        clock.t = 10
        engine.endByItself() // headset plugged in
        #expect(await waitUntil { engine.starts == 2 && v.status == .listening })
        #expect(v.isOn && v.problem == nil)
        engine.send(.transcript(VoiceTranscript("pick", isFinal: true)))
        #expect(await waitUntil { ran.all == [[.key(.pick)]] }, "the new run listens")
        v.turnOff()
    }

    @Test func runEndingRepeatedlyTurnsVoiceOff() async {
        let (v, engine, _, _) = make()
        await v.turnOn()
        for n in 1...3 {
            #expect(await waitUntil { engine.starts == n && engine.session != nil && v.status == .listening })
            engine.endByItself()
        }
        #expect(await waitUntil { v.problem != nil })
        #expect(!v.isOn)
        if case .unavailable(let m) = v.problem { #expect(m.contains("dropping out")) } else { Issue.record("no problem") }
    }

    @Test func supersededStartWhileCurrentRestarts() async {
        let engine = FakeVoiceEngine()
        engine.startError = VoiceEngineError.superseded
        let (v, _, _, _) = make(engine)
        await v.turnOn()
        #expect(await waitUntil { engine.starts >= 2 })
        engine.startError = nil
        #expect(await waitUntil { v.status == .listening })
        #expect(v.problem == nil)
        v.turnOff()
    }

    @Test func finalsRunAndEcho() async {
        let (v, engine, ran, _) = make()
        v.echoDuration = 60
        v.sleep = { try? await Task.sleep(for: .seconds($0)) }
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        engine.send(.transcript(VoiceTranscript("five next", isFinal: true)))
        #expect(await waitUntil { v.echo?.text == "★★★★★ · Next" })
        #expect(ran.all == [[rate(5), next]])
        engine.send(.transcript(VoiceTranscript("what's for dinner", isFinal: true)))
        engine.send(.transcript(VoiceTranscript("pick", isFinal: true)))
        #expect(await waitUntil { ran.all.count == 2 })
        #expect(ran.all.last == [.key(.pick)], "conversation in between did nothing")
        v.turnOff()
    }

    @Test func contradictedFinalShowsHowToTakeItBack() async {
        let (v, engine, ran, clock) = make()
        v.echoDuration = 60
        v.sleep = { try? await Task.sleep(for: .seconds($0)) }
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        engine.send(.transcript(VoiceTranscript("zoom in", confidence: 0.9)))
        #expect(await waitUntil { v.tracker.pending != nil })
        clock.t = 1
        v.tick()
        #expect(ran.all == [[.menu(.zoomIn)]])
        engine.send(.transcript(VoiceTranscript("zoom in on the dog", isFinal: true)))
        // review-g L8: a zoom has nothing to undo, so the note doesn't suggest it.
        #expect(await waitUntil { v.echo?.text == "Heard “zoom in on the dog”: nothing more was run" })
        v.turnOff()
    }

    @Test func echoFades() async {
        let (v, _, _, _) = make()
        v.show("Next")
        #expect(v.echo?.text == "Next")
        #expect(await waitUntil { v.echo == nil })
    }

    @Test func nothingRunEchoesNothing() async {
        let (v, engine, _, _) = make()
        var calls = 0
        v.perform = { _, _ in
            calls += 1
            return nil
        }
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        engine.send(.transcript(VoiceTranscript("next", isFinal: true)))
        #expect(await waitUntil { calls == 1 })
        #expect(v.echo == nil)
        v.turnOff()
    }

    @Test func settledPartialFiresWithoutWaitingForTheFinal() async {
        let (v, engine, ran, clock) = make()
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        engine.send(.transcript(VoiceTranscript("five stars", confidence: 0.9)))
        await settle()
        #expect(ran.all.isEmpty, "not settled at t = 0")
        clock.t = 1
        engine.send(.transcript(VoiceTranscript("five stars", confidence: 0.9))) // the same partial again
        #expect(await waitUntil { ran.all == [[rate(5)]] })
        engine.send(.transcript(VoiceTranscript("five stars", isFinal: true)))
        await settle()
        #expect(ran.all == [[rate(5)]], "one utterance, one command")
        v.turnOff()
    }

    @Test func staleTickDoesNothing() async {
        let (v, engine, ran, clock) = make()
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        engine.send(.transcript(VoiceTranscript("zoom in", confidence: 0.9)))
        await settle()
        clock.t = 5
        v.tick(generation: -1)
        #expect(ran.all.isEmpty, "a tick from an older run fires nothing")
        v.tick()
        #expect(ran.all == [[.menu(.zoomIn)]])
        v.turnOff()
    }

    @Test func pausesWhileInactiveOrTyping() async {
        let (v, engine, ran, _) = make()
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        v.setAppActive(false)
        #expect(v.status == .paused(.inactive))
        #expect(await waitUntil { engine.stops == [1] }, "the mic closes while in the background")
        v.setAppActive(false)
        v.setAppActive(true)
        #expect(await waitUntil { v.status == .listening && engine.session == 2 })
        v.setTyping(true)
        #expect(v.status == .paused(.typing))
        #expect(await waitUntil { engine.stops == [1, 2] })
        engine.send(.transcript(VoiceTranscript("next", isFinal: true)))
        await settle()
        #expect(ran.all.isEmpty, "nothing runs while typing")
        v.setTyping(true)
        v.setTyping(false)
        #expect(await waitUntil { v.status == .listening && engine.session == 3 })
        v.turnOff()
    }

    @Test func pushToTalkListensOnlyWhileHeld() async {
        let (v, engine, ran, _) = make()
        #expect(!v.handlePushToTalk(code: 61, pressed: true), "off: not consumed")
        v.pushToTalk = true
        await v.turnOn()
        #expect(v.status == .paused(.waitingForKey))
        #expect(engine.starts == 0, "the mic stays closed until the key")
        #expect(!v.handlePushToTalk(code: 50, pressed: true), "another key")
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening && engine.session == 1 })
        #expect(v.handlePushToTalk(code: 61, pressed: true), "repeat: consumed, nothing new")
        engine.send(.transcript(VoiceTranscript("five stars")))
        await settle()
        #expect(ran.all.isEmpty, "a settled partial waits out its settle time (the clock hasn't moved)")
        #expect(v.handlePushToTalk(code: 61, pressed: false))
        #expect(ran.all == [[rate(5)]], "released: fires what was heard at once")
        #expect(await waitUntil { engine.finishes == [1] })
        #expect(engine.stops.isEmpty, "finish, not cancel: the recognizer completes the utterance")
        #expect(v.status == .paused(.waitingForKey))
        v.pushToTalk = false
        #expect(await waitUntil { v.status == .listening }, "switching to continuous listens now")
        v.turnOff()
    }

    /// REVERIFY: key tapped and released while the engine is still starting.
    @Test func reverifyPushToTalkTapWhileStartingClosesTheMic() async {
        let engine = FakeVoiceEngine()
        engine.gated = true
        let (v, _, ran, _) = make(engine)
        v.pushToTalk = true
        await v.turnOn()
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { engine.isWaiting })
        #expect(v.handlePushToTalk(code: 61, pressed: false)) // released before the mic opened
        engine.release()
        await settle()
        #expect(engine.stops.count + engine.finishes.count == 1, "the late session is ended (mic closed)")
        v.turnOff()
        await settle()
        #expect(engine.stops.count + engine.finishes.count == 1, "turning voice off closes the mic")
        engine.send(.transcript(VoiceTranscript("reject", isFinal: true)))
        await settle()
        #expect(ran.all.isEmpty, "nothing runs with the key up and voice off")
    }

    /// Pressed again before the last release's final arrived: that final still runs,
    /// for its own run (only what its partials didn't fire), and the new run is untouched.
    @Test func pushToTalkLateFinalRunsForItsOwnRun() async {
        let engine = FakeVoiceEngine()
        engine.holdsFinishing = true
        let (v, _, ran, _) = make(engine)
        v.pushToTalk = true
        await v.turnOn()
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening && engine.session == 1 })
        engine.send(.transcript(VoiceTranscript("five stars")))
        #expect(await waitUntil { v.tracker.pending != nil })
        #expect(v.handlePushToTalk(code: 61, pressed: false))
        #expect(ran.all == [[rate(5)]])
        #expect(await waitUntil { engine.finishes == [1] })
        #expect(v.handlePushToTalk(code: 61, pressed: true)) // again, before run 1's final
        #expect(await waitUntil { v.status == .listening && engine.session == 2 })
        engine.send(.transcript(VoiceTranscript("pick")))
        #expect(await waitUntil { v.tracker.pending != nil })
        engine.deliverLateFinal(VoiceTranscript("five stars reject", isFinal: true), session: 1)
        #expect(await waitUntil(timeout: .seconds(2)) { ran.all.count == 2 }, "run 1's final isn't dropped")
        #expect(ran.all == [[rate(5)], [.key(.reject)]], "only what run 1's partials didn't fire")
        #expect(v.tracker.pending != nil, "run 2's partial is untouched")
        #expect(v.handlePushToTalk(code: 61, pressed: false))
        // review-g H4: a release on a bare verb ("pick" may become "pick the next two") waits for the final.
        #expect(ran.all.count == 2)
        #expect(await waitUntil { engine.finishes == [1, 2] })
        engine.deliverLateFinal(VoiceTranscript("pick", isFinal: true), session: 2)
        #expect(await waitUntil(timeout: .seconds(2)) { ran.all.count == 3 })
        #expect(ran.all.last == [.key(.pick)])
        v.turnOff()
    }

    /// REVERIFY: run 2's partial fires before run 1's late final: spoken order is kept,
    /// and the dropped command says so.
    @Test func reverifyLateFinalAfterNewerRunFiredKeepsSpokenOrder() async {
        let engine = FakeVoiceEngine()
        engine.holdsFinishing = true
        let (v, _, ran, clock) = make(engine)
        v.echoDuration = 60
        v.sleep = { try? await Task.sleep(for: .seconds($0)) }
        v.pushToTalk = true
        await v.turnOn()
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening && engine.session == 1 })
        engine.send(.transcript(VoiceTranscript("five stars")))
        #expect(await waitUntil { v.tracker.pending != nil })
        #expect(v.handlePushToTalk(code: 61, pressed: false))
        #expect(await waitUntil { engine.finishes == [1] })
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening && engine.session == 2 })
        engine.send(.transcript(VoiceTranscript("pick", confidence: 0.9)))
        #expect(await waitUntil { v.tracker.pending != nil })
        clock.t += 1
        v.tick() // run 2's "pick" settles and fires
        #expect(ran.all == [[rate(5)], [.key(.pick)]])
        engine.deliverLateFinal(VoiceTranscript("five stars reject", isFinal: true), session: 1)
        let note = "“\(VoiceAction.key(.reject).label)” not applied — say it again"
        #expect(await waitUntil(timeout: .seconds(2)) { v.echo?.text == note }, "the dropped command is said")
        #expect(ran.all == [[rate(5)], [.key(.pick)]], "run 1's reject would now act after the later pick: \(ran.all)")
        v.turnOff()
    }

    /// Run 2's command couldn't run (nothing to pick, a disabled item): run 1's late
    /// final is still in spoken order, so it runs.
    @Test func lateFinalRunsWhenTheNewerRunRanNothing() async {
        let engine = FakeVoiceEngine()
        engine.holdsFinishing = true
        let (v, _, ran, clock) = make(engine)
        v.perform = { actions, _ in
            ran.all.append(actions)
            return actions == [.key(.pick)] ? nil : actions.map(\.label).joined(separator: " · ")
        }
        v.pushToTalk = true
        await v.turnOn()
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening && engine.session == 1 })
        engine.send(.transcript(VoiceTranscript("five stars")))
        #expect(await waitUntil { v.tracker.pending != nil })
        #expect(v.handlePushToTalk(code: 61, pressed: false))
        #expect(await waitUntil { engine.finishes == [1] })
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening && engine.session == 2 })
        engine.send(.transcript(VoiceTranscript("pick", confidence: 0.9)))
        #expect(await waitUntil { v.tracker.pending != nil })
        clock.t += 1
        v.tick() // run 2's "pick" fires but can't run
        #expect(ran.all == [[rate(5)], [.key(.pick)]])
        engine.deliverLateFinal(VoiceTranscript("five stars reject", isFinal: true), session: 1)
        #expect(await waitUntil(timeout: .seconds(2)) { ran.all.count == 3 }, "run 1's final isn't dropped")
        #expect(ran.all == [[rate(5)], [.key(.pick)], [.key(.reject)]])
        v.turnOff()
    }

    /// REVERIFY: a late final after voice is turned off runs nothing.
    @Test func reverifyLateFinalAfterTurnOffRunsNothing() async {
        let engine = FakeVoiceEngine()
        engine.holdsFinishing = true
        let (v, _, ran, _) = make(engine)
        v.pushToTalk = true
        await v.turnOn()
        #expect(v.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { v.status == .listening && engine.session == 1 })
        #expect(v.handlePushToTalk(code: 61, pressed: false))
        #expect(await waitUntil { engine.finishes == [1] })
        v.turnOff()
        engine.deliverLateFinal(VoiceTranscript("reject", isFinal: true), session: 1)
        await settle()
        #expect(ran.all.isEmpty)
    }

    @MainActor final class SleepLog {
        var waits: [Double] = []
        let clock: Clock
        init(_ clock: Clock) { self.clock = clock }
    }

    /// The tick's sleep woke before the deadline on `now`'s clock: it waits out the rest.
    @Test func earlyTickWaitsOutTheRest() async {
        let (v, engine, ran, clock) = make()
        let log = SleepLog(clock)
        v.echoDuration = 60
        v.sleep = { @MainActor w in
            guard w != 60 else { return } // the echo's fade, not a tick
            log.waits.append(w)
            log.clock.t += log.waits.count == 1 ? w / 2 : w + 0.01 // the first wakes halfway
        }
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        engine.send(.transcript(VoiceTranscript("zoom in", confidence: 0.9)))
        #expect(await waitUntil(timeout: .seconds(2)) { ran.all == [[.menu(.zoomIn)]] }, "fires after the rest")
        #expect(log.waits.count == 2 && abs(log.waits[1] - log.waits[0] / 2) < 1e-9)
        v.turnOff()
    }

    @Test func pushToTalkKeyTypesWhileATextFieldHasFocus() async {
        let (v, _, _, _) = make()
        v.pushToTalk = true
        v.pushToTalkKey = .backquote
        await v.turnOn()
        v.setTyping(true)
        #expect(!v.handlePushToTalk(code: 50, pressed: true), "the ` goes to the field")
        v.setTyping(false)
        #expect(v.handlePushToTalk(code: 50, pressed: true))
        v.turnOff()
    }

    @Test func pushToTalkKeys() {
        #expect(PushToTalkKey.rightOption.keyCode == 61 && PushToTalkKey.rightOption.isModifier)
        #expect(PushToTalkKey.backquote.keyCode == 50 && !PushToTalkKey.backquote.isModifier)
        #expect(PushToTalkKey.rightOption.title.contains("⌥") && PushToTalkKey.backquote.title.contains("`"))
        #expect(PushToTalkKey.allCases.allSatisfy { $0.id == $0.rawValue })
    }

    @Test func problemsAndText() {
        #expect(VoiceProblem.microphoneDenied.message.contains("Microphone"))
        #expect(VoiceProblem.speechDenied.message.contains("Speech Recognition"))
        #expect(VoiceProblem.unavailable("No model").message == "No model")
        #expect(VoiceProblem.unavailable("No model").title == "Voice commands aren't available")
        #expect(VoiceProblem.microphoneDenied.id != VoiceProblem.speechDenied.id)
        #expect(VoiceText.help(.off).hasPrefix("Voice commands (⌃⌘V)"))
        #expect(VoiceText.help(.listening).hasPrefix("Listening"))
        #expect(VoiceText.help(.starting).contains("starting"))
        #expect(VoiceText.help(.paused(.typing)).hasPrefix("Voice paused while you type"))
        #expect(VoiceText.help(.paused(.waitingForKey)).hasPrefix("Paused"))
        #expect(VoiceText.accessibilityLabel(.off) == "Voice commands off")
        #expect(VoiceText.accessibilityLabel(.starting) == "Voice commands starting")
        #expect(VoiceText.accessibilityLabel(.listening) == "Voice commands listening")
        #expect(VoiceText.accessibilityLabel(.paused(.inactive)) == "Voice commands paused")
        #expect(VoiceText.pauseNote(.listening) == nil && VoiceText.pauseNote(.off) == nil)
        #expect(VoiceText.pauseNote(.paused(.waitingForKey)) == nil, "push-to-talk idle is normal")
        #expect(VoiceText.pauseNote(.paused(.inactive))?.contains("background") == true)
        #expect(VoiceText.pauseNote(.starting) == "Starting voice commands…")
        let note = VoiceText.languageNote
        #expect(note.contains("English") && note.contains("never leaves the Mac") && note.contains("press Return"))
        #expect(note.contains("Settings ▸ AI"), "edit phrases go to the configured language service")
        #expect(!note.contains("SFSpeech") && !note.lowercased().contains("fallback"))
    }
}

// MARK: - Commands on the app

@MainActor
@Suite struct VoiceAppTests {
    private func started() async -> AppModel {
        let m = makeModel()
        await m.start()
        return m
    }

    @Test func ratingUsesTheKeyPathWithUndo() async {
        let m = await started()
        let id = m.focusedID!
        let before = m.photo(id)?.rating
        #expect(m.performVoice(VoiceCommandParser.parse("five")!) == "★★★★★")
        #expect(m.photo(id)?.rating == 5)
        #expect(m.history.undoLabel == "Rate 5", "the same undo step as the 5 key")
        #expect(m.performVoice(VoiceCommandParser.parse("undo")!) == "Undo")
        #expect(m.photo(id)?.rating == before)
        m.stop()
    }

    @Test func chainRatesThenAdvances() async {
        let m = await started()
        let ids = m.visibleIDs
        #expect(m.performVoice([rate(4), next]) == "★★★★ · Next")
        #expect(m.photo(ids[0])?.rating == 4)
        #expect(m.focusedID == ids[1])
        m.stop()
    }

    @Test func chainWithAutoAdvanceStepsOnce() async {
        let m = await started()
        m.settings.autoAdvance = true
        let ids = m.visibleIDs
        #expect(m.performVoice([rate(3), next]) == "★★★ · Next")
        #expect(m.photo(ids[0])?.rating == 3)
        #expect(m.focusedID == ids[1], "the rating advanced; “next” isn't a second step")
        m.performVoice([.key(.pick), next, next])
        #expect(m.focusedID == ids[3], "a later next still moves")
        m.performVoice([.key(.tag(.blue)), next])
        #expect(m.focusedID == ids[4], "tags don't auto-advance, so next moves")
        m.stop()
    }

    /// HIGH-1: "five" fires from a partial, "next" from a later one: still one step.
    @Test func splitPartialsWithAutoAdvanceStepOnce() async {
        let m = await started()
        m.settings.autoAdvance = true
        let ids = m.visibleIDs
        let engine = FakeVoiceEngine()
        var t = 0.0
        m.voice.engine = engine
        m.voice.permissions = FakeVoicePermissions()
        m.voice.now = { t }
        m.voice.sleep = { _ in }
        await m.voice.turnOn()
        #expect(await waitUntil { m.voice.status == .listening })
        engine.send(.transcript(VoiceTranscript("five stars", confidence: 0.9)))
        #expect(await waitUntil { m.voice.tracker.pending != nil })
        t = 1
        m.voice.tick()
        #expect(m.photo(ids[0])?.rating == 5 && m.focusedID == ids[1], "rated, auto-advanced")
        engine.send(.transcript(VoiceTranscript("five stars next one", confidence: 0.9)))
        #expect(await waitUntil { m.voice.tracker.pending != nil })
        t = 2
        m.voice.tick()
        #expect(m.voice.tracker.fired.count == 2)
        #expect(m.focusedID == ids[1], "the next in the same utterance isn't a second step")
        engine.send(.transcript(VoiceTranscript("five stars next one", isFinal: true)))
        #expect(await waitUntil { m.voice.tracker.fired.isEmpty })
        #expect(m.focusedID == ids[1])
        // A new utterance's "next" moves again.
        t = 3
        engine.send(.transcript(VoiceTranscript("next", isFinal: true)))
        #expect(await waitUntil { m.focusedID == ids[2] })
        m.voice.turnOff()
        m.stop()
    }

    /// HIGH-1 on the push-to-talk path: "five" flushed at release, "next" from the final.
    @Test func pushToTalkFlushThenFinalStepsOnce() async {
        let m = await started()
        m.settings.autoAdvance = true
        let ids = m.visibleIDs
        let engine = FakeVoiceEngine()
        m.voice.engine = engine
        m.voice.permissions = FakeVoicePermissions()
        m.voice.sleep = { _ in }
        m.voice.now = { 0 } // a slow machine must not expire the chain between "five" and its final
        let perform = m.voice.perform
        var performed: [[VoiceAction]] = []
        m.voice.perform = { actions, chain in
            performed.append(actions)
            return perform(actions, &chain)
        }
        m.voice.pushToTalk = true
        await m.voice.turnOn()
        #expect(m.voice.handlePushToTalk(code: 61, pressed: true))
        #expect(await waitUntil { m.voice.status == .listening })
        engine.send(.transcript(VoiceTranscript("five stars")))
        #expect(await waitUntil { m.voice.tracker.pending != nil })
        // Release fires the settled "five stars" now (review-g2 H4': only a settled partial
        // fires at release); the recognizer's final arrives on the finishing stream.
        engine.holdsFinishing = true
        #expect(m.voice.handlePushToTalk(code: 61, pressed: false))
        #expect(m.photo(ids[0])?.rating == 5 && m.focusedID == ids[1], "flushed at release, auto-advanced")
        #expect(await waitUntil { engine.finishes == [1] })
        engine.deliverLateFinal(VoiceTranscript("five stars next", isFinal: true), session: 1)
        #expect(await waitUntil { performed.count == 2 }, "the final was processed")
        #expect(performed == [[rate(5)], [next]])
        #expect(m.focusedID == ids[1], "the final's next isn't a second step")
        m.voice.turnOff()
        m.stop()
    }

    @Test func chainExpires() async {
        let m = await started()
        m.settings.autoAdvance = true
        let ids = m.visibleIDs
        let v = m.voice
        var t = 0.0
        v.now = { t }
        let engine = FakeVoiceEngine()
        v.engine = engine
        v.permissions = FakeVoicePermissions()
        v.sleep = { _ in }
        await v.turnOn()
        #expect(await waitUntil { v.status == .listening })
        engine.send(.transcript(VoiceTranscript("five stars", confidence: 0.9)))
        #expect(await waitUntil { v.tracker.pending != nil })
        t = 1
        v.tick()
        #expect(m.focusedID == ids[1])
        t = 10 // long after: the utterance's chain is stale
        engine.send(.transcript(VoiceTranscript("five stars next", isFinal: true)))
        #expect(await waitUntil { m.focusedID != ids[1] })
        #expect(m.focusedID == ids[2], "after the timeout, next moves")
        // Within the timeout, the final's next is the step the rating already made.
        engine.send(.transcript(VoiceTranscript("five stars", confidence: 0.9)))
        #expect(await waitUntil { v.tracker.pending != nil })
        t = 11
        v.tick()
        #expect(m.focusedID == ids[3])
        engine.send(.transcript(VoiceTranscript("five stars next", isFinal: true)))
        #expect(await waitUntil { v.tracker.fired.isEmpty })
        #expect(m.focusedID == ids[3])
        v.turnOff()
        m.stop()
    }

    @Test func flagsAndTags() async {
        let m = await started()
        let id = m.focusedID!
        m.performVoice([.key(.reject)])
        #expect(m.photo(id)?.pick == .rejected)
        m.performVoice([.key(.unflag)])
        #expect(m.photo(id)?.pick == PickState.none)
        m.performVoice([.key(.tag(.green))])
        #expect(m.photo(id)?.colorTag == .green)
        m.stop()
    }

    @Test func jumps() async {
        let m = await started()
        let ids = m.visibleIDs
        #expect(m.performVoice([.jump(.last)]) == "Last")
        #expect(m.focusedID == ids.last)
        m.performVoice([.jump(.first)])
        #expect(m.focusedID == ids[0])
        m.updatePhotos(ids, label: "t") { $0.rating = 2; $0.pick = .none }
        m.updatePhotos([ids[3]], label: "t") { $0.rating = 0 }
        m.updatePhotos([ids[5]], label: "t") { $0.pick = .picked }
        m.performVoice([.jump(.nextUnrated)])
        #expect(m.focusedID == ids[3])
        m.performVoice([.jump(.nextPick)])
        #expect(m.focusedID == ids[5])
        #expect(m.performVoice([.jump(.nextPick)]) == nil, "none left")
        #expect(m.toast?.message == "No more picked photos")
        #expect(m.performVoice([.jump(.nextUnrated)]) == nil)
        #expect(m.toast?.message == "No more unrated photos")
        #expect(m.focusedID == ids[5])
        m.stop()
    }

    @Test func viewCommands() async {
        let m = await started()
        m.performVoice([.key(.gridMode)])
        #expect(m.browserMode == .grid)
        m.performVoice([.key(.viewerMode)])
        #expect(m.browserMode == .filmstrip)
        m.performVoice([.setPanelsHidden(true)])
        #expect(m.panelsHidden)
        m.performVoice([.setPanelsHidden(true)])
        #expect(m.panelsHidden, "hide twice stays hidden")
        m.performVoice([.setPanelsHidden(false)])
        #expect(!m.panelsHidden)
        m.performVoice([.setFullScreen(false)])
        #expect(!m.viewer.state.fullScreen, "exit when not full screen does nothing")
        m.performVoice([.key(.toggleFullScreen)])
        #expect(m.viewer.state.fullScreen)
        m.performVoice([.setFullScreen(false)])
        #expect(!m.viewer.state.fullScreen)
        m.viewer.state.viewSize = CGSize(width: 1224, height: 824)
        #expect(m.performVoice([.menu(.zoomActual)]) == "100%")
        #expect(m.viewer.state.zoom != .fit)
        m.performVoice([.menu(.zoomFit)])
        #expect(m.viewer.state.zoom == .fit)
        m.stop()
    }

    @Test func menuCommandsRunOnlyWhenTheirItemIsEnabled() async {
        let m = await started()
        #expect(m.performVoice([.menu(.applyAdjustments)]) == nil, "nothing copied: the menu item is off")
        #expect(m.sheet == nil)
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.3 } // something to copy
        #expect(m.performVoice([.menu(.copyAdjustments)]) == "Copy adjustments")
        #expect(m.clipboard?.sourceID == m.focusedID && m.sheet == nil, "the same one-click copy as ⇧⌘C")
        #expect(m.performVoice([.menu(.applyAdjustments)]) == nil, "only the copied photo is selected")
        m.perform(.move(.right, extend: false))
        #expect(m.performVoice([.menu(.applyAdjustments)]) == "Apply adjustments", "now there is somewhere to apply")
        m.sheet = .export
        #expect(m.performVoice([next]) == nil, "a sheet owns the keyboard, and voice")
        m.stop()
    }

    @Test func editOpensEditWithWordsForConfirmation() async {
        let m = await started()
        let edit = m.edit(for: m.focusedID!)
        #expect(m.performVoice([.edit("warmer")]) == "Edit: “warmer”, press Return")
        #expect(m.commandBar.isPresented && m.commandBar.text == "warmer", "prefilled, not applied")
        #expect(m.commandBar.submission == "warmer")
        #expect(m.edit(for: m.focusedID!) == edit && m.commandBar.lastResult == nil, "nothing ran or was sent")
        #expect(m.performVoice([.edit("cooler")]) == nil, "not while the bar is open")
        #expect(m.commandBar.text == "warmer")
        m.commandBar.dismiss()
        m.stop()
    }

    @Test func editNeedsAPhoto() {
        let m = makeModel()
        #expect(m.performVoice([.edit("warmer")]) == nil)
        #expect(!m.commandBar.isPresented)
    }

    @Test func controllerRunsCommandsOnTheModel() async {
        let m = await started()
        let id = m.focusedID!
        let engine = FakeVoiceEngine()
        m.voice.engine = engine
        m.voice.permissions = FakeVoicePermissions()
        await m.voice.turnOn()
        #expect(await waitUntil { m.voice.status == .listening })
        engine.send(.transcript(VoiceTranscript("two stars", isFinal: true)))
        #expect(await waitUntil { m.photo(id)?.rating == 2 })
        #expect(m.voice.echo?.text == "★★")
        m.voice.turnOff()
        m.stop()
    }

    @Test func menuToggleAndShortcut() async {
        let m = makeModel()
        #expect(MenuCommand.voiceCommands.shortcut == .init(key: "v", modifiers: [.command, .control]))
        #expect(CommandCatalog.title(.voiceCommands, model: m) == "Voice Commands")
        #expect(CommandCatalog.isEnabled(.voiceCommands, model: m))
        // ⌃⌘V never reaches the culling keys.
        #expect(KeyboardRouter.command(for: KeyInput("v", command: true, control: true)) == nil)
        m.voice.permissions = FakeVoicePermissions(mic: .denied)
        CommandCatalog.perform(.voiceCommands, model: m)
        #expect(m.voice.isOn, "on at once")
        #expect(await waitUntil { m.voice.problem == .microphoneDenied && !m.voice.isOn })
    }

    @Test func settingsPersistAndReachTheController() {
        let s = makeSettings()
        #expect(!s.voicePushToTalk && s.voicePushToTalkKey == .rightOption)
        s.voicePushToTalk = true
        s.voicePushToTalkKey = .backquote
        let again = AppSettings(defaults: s.userDefaults)
        #expect(again.voicePushToTalk && again.voicePushToTalkKey == .backquote)
        let m = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                         renderer: FakeRenderer(), importer: MockImportService(), archive: MockArchiveService(target: ""),
                         ai: MockAIService(), settings: again)
        #expect(m.voice.pushToTalk && m.voice.pushToTalkKey == .backquote)
    }

    @Test func cheatSheetListsVoice() {
        let voice = KeyboardRouter.cheatSheet().first { $0.section == "Voice" }
        #expect(voice?.rows.first?.keys == "⌃⌘V")
        #expect(voice?.rows.contains { $0.keys == "“five next”" } == true)
    }
}
