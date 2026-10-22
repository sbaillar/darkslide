import Foundation
import Observation

/// What a speech engine reports while it listens.
public enum VoiceEngineEvent: Equatable, Sendable {
    case transcript(VoiceTranscript)
    /// Listening stopped on its own (model error); voice commands turn off.
    case failed(String)
}

public enum VoiceEngineError: Error, Equatable, Sendable {
    /// On-device recognition can't run here; the message says why. Never falls back to a server.
    case unavailable(String)
    /// A newer `start` replaced this one before it was ready.
    case superseded
}

/// One listening run: its token and its events, in order. The stream ends when the
/// run is finished or stopped, or by itself when the audio input changes (a
/// headset plugged in): the controller then starts a new run.
public struct VoiceSession: Sendable {
    public let id: Int
    public let events: AsyncStream<VoiceEngineEvent>

    public init(id: Int, events: AsyncStream<VoiceEngineEvent>) {
        self.id = id
        self.events = events
    }
}

/// The speech recognizer behind voice commands (on device only). A seam: tests use a fake.
/// `finish` and `stop` act only on the session they name, so a late call from an older
/// run never touches a newer one.
public protocol VoiceEngine: Sendable {
    /// Open the microphone and start recognizing, biased toward `vocabulary`.
    func start(vocabulary: [String]) async throws -> VoiceSession
    /// Close the microphone now, deliver what was already heard, then end the stream.
    func finish(_ session: Int) async
    /// Close the microphone now and drop everything.
    func stop(_ session: Int) async
}

/// Carried between the commands of one utterance: a rating that auto-advanced makes
/// the "next" after it (even from a later partial) not a second step.
public struct VoiceChain: Equatable, Sendable {
    public var advanced = false
    public init() {}
}

public enum VoicePermission: Equatable, Sendable {
    case granted, notDetermined, denied
}

/// Microphone and speech-recognition consent. A seam: tests never see a prompt.
public protocol VoicePermissions: Sendable {
    @MainActor func microphone() -> VoicePermission
    @MainActor func speech() -> VoicePermission
    func requestMicrophone() async -> Bool
    func requestSpeech() async -> Bool
}

/// Why voice commands aren't listening although they are on.
public enum VoicePause: Equatable, Sendable {
    /// Darkslide isn't the active app.
    case inactive
    /// A text field has the keyboard.
    case typing
    /// Push-to-talk: waiting for the key.
    case waitingForKey
}

/// A problem that turned voice commands off; the view offers System Settings for denials.
public enum VoiceProblem: Equatable, Sendable, Identifiable {
    case microphoneDenied, speechDenied
    case unavailable(String)

    public var id: String { message }

    public var title: String {
        switch self {
        case .microphoneDenied: "Darkslide can't use the microphone"
        case .speechDenied: "Speech recognition is turned off for Darkslide"
        case .unavailable: "Voice commands aren't available"
        }
    }

    public var message: String {
        switch self {
        case .microphoneDenied:
            "Voice commands need the microphone. Allow Darkslide in System Settings ▸ Privacy & Security ▸ Microphone, then turn voice commands on again."
        case .speechDenied:
            "Voice commands need speech recognition. Allow Darkslide in System Settings ▸ Privacy & Security ▸ Speech Recognition, then turn voice commands on again."
        case .unavailable(let why): why
        }
    }

    /// The System Settings pane that fixes it (`nil`: nothing there helps).
    public var settingsURL: URL? {
        switch self {
        case .microphoneDenied: URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        case .speechDenied: URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition")
        case .unavailable: nil
        }
    }
}

/// What the HUD shows for a moment after a command runs.
public struct VoiceEcho: Equatable, Sendable, Identifiable {
    public let id = UUID()
    public var text: String
}

/// Voice commands: on/off, push-to-talk, pausing, permissions, and turning what the
/// recognizer hears into commands (`VoiceUtteranceTracker`). Commands run through
/// `perform`, which `AppModel` routes onto the keyboard's and menus' paths.
@MainActor @Observable
public final class VoiceController {
    public enum Status: Equatable, Sendable {
        case off
        /// Asking for permission or loading the speech model.
        case starting
        case listening
        case paused(VoicePause)
    }

    /// The user's switch (toolbar mic, View ▸ Voice Commands, ⌃⌘V).
    public private(set) var isOn = false
    public private(set) var problem: VoiceProblem?
    /// The last commands heard, shown briefly.
    public private(set) var echo: VoiceEcho?
    /// Push-to-talk instead of continuous listening.
    public var pushToTalk = false { didSet { if pushToTalk != oldValue { reconcile() } } }
    public var pushToTalkKey: PushToTalkKey = .rightOption
    /// The user's own phrases (Settings ▸ Voice); the run listening now hears them at once.
    public var customPhrases: [String: VoiceAction] = [:] { didSet { current?.tracker.custom = customPhrases } }
    /// The Voice Commands sheet ("what can I say?", View ▸ What Can I Say?, the mic's menu).
    public var helpShown = false

    @ObservationIgnored public var engine: any VoiceEngine
    @ObservationIgnored public var permissions: any VoicePermissions
    /// Runs commands; returns what the HUD echoes (`nil`: nothing ran).
    @ObservationIgnored public var perform: @MainActor ([VoiceAction], inout VoiceChain) -> String? = { _, _ in nil }
    /// Seconds on a monotonic clock (tests drive it).
    @ObservationIgnored var now: @MainActor () -> Double = { ProcessInfo.processInfo.systemUptime }
    /// Waits; returns early when its task is cancelled (callers check).
    @ObservationIgnored var sleep: @Sendable (Double) async -> Void = { try? await Task.sleep(for: .seconds($0)) }
    @ObservationIgnored public var echoDuration = 1.4
    /// An utterance's chain state is dropped after this long without a command (seconds).
    @ObservationIgnored var chainTimeout = 2.0

    private(set) var appActive = true
    private(set) var typing = false
    private(set) var keyHeld = false
    /// Permission is being asked.
    private(set) var preparing = false
    /// The engine is (being) started.
    private(set) var engineRunning = false
    /// The engine's start returned: the microphone is open.
    private(set) var engineStarted = false
    /// Numbers the runs: a tick scheduled for an older run does nothing.
    @ObservationIgnored private var generation = 0
    /// The run listening now (or starting, or finishing after a push-to-talk release).
    @ObservationIgnored private var current: Run?
    /// Bumped by each turn-on: a permission answer for an older one is dropped.
    @ObservationIgnored private var turnOnToken = 0
    @ObservationIgnored private var startedAt = 0.0
    /// Runs that ended by themselves right after starting, in a row.
    @ObservationIgnored private var quickEnds = 0
    @ObservationIgnored private var lastRunAt = -Double.infinity
    /// The newest run that ran a command.
    @ObservationIgnored private var lastRanGeneration = 0
    @ObservationIgnored private var tickTask: Task<Void, Never>?
    @ObservationIgnored private var echoTask: Task<Void, Never>?

    /// One listening run: its engine session, what it heard and its chain. A push-to-talk
    /// run whose key went up lives on until the recognizer's final arrives, even after a
    /// newer run starts, so that final fires what it adds for the run it was said in.
    @MainActor private final class Run {
        let generation: Int
        /// The engine's session (`nil` while it starts).
        var session: Int?
        var tracker: VoiceUtteranceTracker
        var chain = VoiceChain()
        /// The key went up: the mic is closed and the recognizer finishes what was said.
        var finishing = false
        /// Over: its late events and the engine's late answer are dropped.
        var ended = false

        init(generation: Int, mode: VoiceUtteranceTracker.Mode) {
            self.generation = generation
            tracker = VoiceUtteranceTracker(mode: mode)
        }
    }

    /// What the current run has heard (empty between runs).
    var tracker: VoiceUtteranceTracker { current?.tracker ?? VoiceUtteranceTracker() }

    public init(engine: any VoiceEngine = UnavailableVoiceEngine(), permissions: any VoicePermissions = NoVoicePermissions()) {
        self.engine = engine
        self.permissions = permissions
    }

    public var status: Status {
        guard isOn else { return .off }
        if preparing { return .starting }
        if !appActive { return .paused(.inactive) }
        if typing { return .paused(.typing) }
        if pushToTalk, !keyHeld { return .paused(.waitingForKey) }
        return engineStarted ? .listening : .starting
    }

    private var shouldListen: Bool { isOn && !preparing && appActive && !typing && (!pushToTalk || keyHeld) }

    // MARK: - Switching

    /// On or off. The switch flips now (the menu checkmark and toolbar follow at
    /// once); asking for permission continues in the background.
    public func toggle() {
        if isOn {
            turnOff()
        } else if beginTurnOn() {
            let token = turnOnToken
            Task { await completeTurnOn(token) }
        }
    }

    /// Ask for permission the first time, then listen (or wait for the push-to-talk key).
    public func turnOn() async {
        guard beginTurnOn() else { return }
        await completeTurnOn(turnOnToken)
    }

    private func beginTurnOn() -> Bool {
        guard !isOn else { return false }
        problem = nil
        isOn = true
        preparing = true
        turnOnToken += 1
        return true
    }

    private func completeTurnOn(_ token: Int) async {
        let denied = await permissionProblem()
        guard isOn, token == turnOnToken else { return } // turned off (or on again) while asking
        preparing = false
        if let denied {
            problem = denied
            isOn = false
            return
        }
        reconcile()
    }

    /// Stop listening and release the microphone now.
    public func turnOff() {
        isOn = false
        preparing = false
        reconcile()
    }

    public func dismissProblem() { problem = nil }

    private func permissionProblem() async -> VoiceProblem? {
        if permissions.microphone() == .notDetermined { _ = await permissions.requestMicrophone() }
        guard permissions.microphone() == .granted else { return .microphoneDenied }
        if permissions.speech() == .notDetermined { _ = await permissions.requestSpeech() }
        guard permissions.speech() == .granted else { return .speechDenied }
        return nil
    }

    // MARK: - Pausing

    public func setAppActive(_ active: Bool) {
        guard active != appActive else { return }
        appActive = active
        reconcile()
    }

    public func setTyping(_ typing: Bool) {
        guard typing != self.typing else { return }
        self.typing = typing
        reconcile()
    }

    /// A key event for push-to-talk; returns whether it was the push-to-talk key (consumed).
    public func handlePushToTalk(code: UInt16, pressed: Bool) -> Bool {
        guard isOn, pushToTalk, !typing || keyHeld, code == pushToTalkKey.keyCode else { return false }
        if pressed != keyHeld {
            keyHeld = pressed
            reconcile(endOfSpeech: !pressed)
        }
        return true
    }

    /// Start or stop the engine to match the switch, focus and push-to-talk.
    /// `endOfSpeech`: the push-to-talk key went up, so what was heard is complete.
    private func reconcile(endOfSpeech: Bool = false) {
        if shouldListen, !engineRunning {
            engineRunning = true
            generation += 1
            let r = Run(generation: generation, mode: pushToTalk ? .pushToTalk : .continuous)
            r.tracker.custom = customPhrases
            current = r
            Task { await self.listen(r) }
        } else if !shouldListen, engineRunning {
            engineRunning = false
            engineStarted = false
            tickTask?.cancel()
            guard let r = current else { return }
            if endOfSpeech, let id = r.session {
                // The speaker is done: what is pending fires now, and the recognizer finishes
                // what was said (the mic closes now); the listen loop runs its final.
                run(r.tracker.flush(), in: r)
                r.finishing = true
                Task { await engine.finish(id) }
            } else {
                // Also a key tapped while the engine starts: nothing was heard yet.
                endRun(r)
            }
        }
    }

    /// End `r` and close its microphone. Still starting (no session yet): the listen
    /// loop stops the session when it arrives.
    private func endRun(_ r: Run) {
        guard !r.ended else { return }
        r.ended = true
        if current === r { current = nil }
        if let id = r.session { Task { await engine.stop(id) } }
    }

    private func listen(_ r: Run) async {
        let started: VoiceSession
        do {
            started = try await engine.start(vocabulary: VoiceCommandParser.vocabulary + customPhrases.keys.sorted())
        } catch {
            guard !r.ended else { return }
            if error as? VoiceEngineError == .superseded { // the input changed while starting
                endRun(r)
                engineRunning = false
                return reconcile()
            }
            let message = switch error as? VoiceEngineError {
            case .unavailable(let m): m
            default: error.localizedDescription
            }
            fail(with: message)
            return
        }
        guard !r.ended else {
            await engine.stop(started.id) // turned off (or the key went up) while starting
            return
        }
        r.session = started.id
        engineStarted = true
        startedAt = now()
        for await event in started.events {
            guard !r.ended else { return }
            switch event {
            case .transcript(let t):
                run(r.tracker.receive(t, at: now()), in: r)
                if t.isFinal {
                    r.chain = VoiceChain()
                    if let heard = r.tracker.takeContradiction() {
                        show(r.tracker.contradictionUndoable ? "Heard “\(heard)”: say “undo” to take it back"
                            : "Heard “\(heard)”: nothing more was run")
                    }
                }
            case .failed(let why):
                fail(with: why, in: r)
                return
            }
            if !r.finishing { scheduleTick(r) }
        }
        guard !r.ended else { return }
        run(r.tracker.flush(), in: r)
        // A release held back an unfinished command for the final, and none came (the
        // recognizer gave up): say so rather than drop it silently, or guess and run it.
        if r.tracker.takeHeldBack() != nil, r.finishing { show(VoiceText.notCaught) }
        r.ended = true
        if current === r { current = nil }
        guard !r.finishing else { return }
        // It ended by itself (the audio input changed): start a new run, unless that
        // keeps happening.
        quickEnds = now() - startedAt < 1 ? quickEnds + 1 : 0
        engineRunning = false
        engineStarted = false
        if quickEnds >= 3 {
            quickEnds = 0
            fail(with: "Voice commands stopped: the microphone keeps dropping out.")
        } else {
            reconcile()
        }
    }

    private func fail(with message: String, in r: Run? = nil) {
        problem = .unavailable(message)
        isOn = false
        engineRunning = false
        engineStarted = false
        if let r { endRun(r) }
        if let current { endRun(current) }
    }

    private func scheduleTick(_ r: Run) {
        tickTask?.cancel()
        guard let deadline = r.tracker.deadline else { return }
        let from = now()
        tickTask = Task { [sleep] in
            await sleep(max(0, deadline - from))
            guard !Task.isCancelled else { return }
            self.tick(generation: r.generation)
            // Woke before the deadline (the sleep's clock isn't `now`'s): wait out the
            // rest. A clock that didn't move at all would only spin.
            let t = self.now()
            if self.current === r, let d = r.tracker.deadline, t < d, t > from { self.scheduleTick(r) }
        }
    }

    func tick(generation gen: Int? = nil) {
        guard let r = current, gen == nil || gen == r.generation else { return }
        run(r.tracker.tick(at: now()), in: r)
    }

    // MARK: - Running

    private func run(_ actions: [VoiceAction], in r: Run) {
        guard !actions.isEmpty, isOn, !r.ended, !typing, appActive else { return }
        // A late final from an older run after a newer run ran something: running it now
        // would act out of spoken order (on the photo the newer commands moved to).
        guard r.generation >= lastRanGeneration else {
            return show("“\(actions.map(\.label).joined(separator: " · "))” not applied — say it again")
        }
        let t = now()
        if t - lastRunAt > chainTimeout { r.chain = VoiceChain() }
        lastRunAt = t
        guard let text = perform(actions, &r.chain) else { return } // nothing ran
        lastRanGeneration = r.generation
        show(text)
    }

    func show(_ text: String) {
        echo = VoiceEcho(text: text)
        echoTask?.cancel()
        echoTask = Task { [sleep, echoDuration] in
            await sleep(echoDuration)
            if !Task.isCancelled { self.echo = nil }
        }
    }
}

/// The key held for push-to-talk.
public enum PushToTalkKey: String, CaseIterable, Sendable, Identifiable {
    /// Right ⌥: types nothing on its own, and no Darkslide shortcut uses the right one alone.
    case rightOption
    /// The ` key (left of 1): no default culling key.
    case backquote

    public var id: String { rawValue }

    /// The hardware key code (layout independent).
    public var keyCode: UInt16 {
        switch self {
        case .rightOption: 61
        case .backquote: 50
        }
    }

    /// A modifier: seen as a flags change, not a key down/up.
    public var isModifier: Bool { self == .rightOption }

    public var title: String {
        switch self {
        case .rightOption: "Right ⌥ Option"
        case .backquote: "` (left of 1)"
        }
    }
}

/// The default engine before the app wires the real one: never listens.
public struct UnavailableVoiceEngine: VoiceEngine {
    public init() {}
    public func start(vocabulary: [String]) async throws -> VoiceSession {
        throw VoiceEngineError.unavailable("Voice commands aren't available in this build.")
    }
    public func finish(_ session: Int) async {}
    public func stop(_ session: Int) async {}
}

/// The default permissions before the app wires the system's: always denied, never prompts.
public struct NoVoicePermissions: VoicePermissions {
    public init() {}
    public func microphone() -> VoicePermission { .denied }
    public func speech() -> VoicePermission { .denied }
    public func requestMicrophone() async -> Bool { false }
    public func requestSpeech() async -> Bool { false }
}

/// Words the voice UI shows (kept out of the views so they're tested).
public enum VoiceText {
    public static func help(_ status: VoiceController.Status) -> String {
        switch status {
        case .off: "Voice commands (⌃⌘V): say “next”, “five”, “pick”, or “what can I say?”. Recognition stays on this Mac."
        case .starting: "Voice commands are starting (⌃⌘V turns them off)"
        case .listening: "Listening for voice commands (⌃⌘V turns them off)"
        case .paused(let why): "\(pauseNote(.paused(why)) ?? "Paused") (⌃⌘V turns voice commands off)"
        }
    }

    public static func accessibilityLabel(_ status: VoiceController.Status) -> String {
        switch status {
        case .off: "Voice commands off"
        case .starting: "Voice commands starting"
        case .listening: "Voice commands listening"
        case .paused: "Voice commands paused"
        }
    }

    /// Why voice commands aren't listening although they're on (`nil`: they are, or they're off).
    public static func pauseNote(_ status: VoiceController.Status) -> String? {
        switch status {
        case .paused(.inactive): "Voice paused while Darkslide is in the background"
        case .paused(.typing): "Voice paused while you type"
        case .starting: "Starting voice commands…"
        case .off, .listening, .paused(.waitingForKey): nil
        }
    }

    /// A push-to-talk command that never got its final.
    public static let notCaught = "Didn’t catch that — say it again"

    public static let languageNote = """
        Voice commands understand English: “next”, “five”, “five next”, “pick”, “red”, “zoom in”, “next 3”, \
        “go to photo 12”, “reject the next 3”; say “what can I say?” for the full list. \
        Recognition runs on this Mac with Apple’s on-device speech model (macOS may download the model once); \
        audio never leaves the Mac, and if on-device recognition isn’t available, voice commands stay off \
        rather than use a server. Only a phrase made entirely of commands does anything: a single word like \
        “next” acts when you finish speaking (at once with push to talk), so talk in the room is ignored. \
        “edit …” opens Edit with Words with your phrase filled in; press Return to apply it. Like a typed \
        instruction, it then goes to the language service chosen in Settings ▸ AI, if one is set up.
        """
}
