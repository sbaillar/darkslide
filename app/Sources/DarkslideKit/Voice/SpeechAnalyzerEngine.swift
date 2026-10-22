@preconcurrency import AVFoundation
import Foundation
import Speech

/// Voice commands on Apple's on-device speech model (SpeechAnalyzer + SpeechTranscriber,
/// macOS 26). Audio and text never leave the Mac: SpeechAnalyzer has no server mode,
/// and when the model can't run here, voice commands are unavailable instead.
///
/// An actor, so starting and stopping the audio engine never blocks the main actor.
/// Each `start` is a session with its own token: a start that a newer one replaced
/// tears down only its own objects, and `finish`/`stop` act only on the session
/// they name. Needs the microphone, so tests use a fake (`VoiceEngine`).
public actor SpeechAnalyzerEngine: VoiceEngine {
    /// One session's objects.
    private final class Run {
        let id: Int
        var audio: AVAudioEngine?
        var analyzer: SpeechAnalyzer?
        var input: AsyncStream<AnalyzerInput>.Continuation?
        var events: AsyncStream<VoiceEngineEvent>.Continuation?
        var results: Task<Void, Never>?
        var observer: NSObjectProtocol?

        init(id: Int) { self.id = id }
    }

    private var current: Run?
    private var latest = 0

    public init() {}

    /// The grammar is English; the system's English variant closest to the user's.
    static func preferredLocale() -> Locale {
        Locale.current.language.languageCode == .english ? Locale.current : Locale(identifier: "en-US")
    }

    public func start(vocabulary: [String]) async throws -> VoiceSession {
        latest += 1
        let run = Run(id: latest)
        // One microphone session at a time. `run` replaces the old one before the await, so
        // the old one's `open`, suspended at an await, fails its next `check` instead of
        // starting an audio engine nobody stops.
        let old = current
        current = run
        if let old { await close(old) }
        do {
            return try await open(run, vocabulary: vocabulary)
        } catch {
            await close(run)
            if current === run { current = nil }
            throw error as? VoiceEngineError
                ?? VoiceEngineError.unavailable("The microphone couldn't start: \(error.localizedDescription)")
        }
    }

    /// Throws once a newer `start` (or a `stop`) replaced `run` during an await.
    private func check(_ run: Run) throws {
        guard current === run else { throw VoiceEngineError.superseded }
    }

    private func open(_ run: Run, vocabulary: [String]) async throws -> VoiceSession {
        guard SpeechTranscriber.isAvailable,
              let locale = await SpeechTranscriber.supportedLocale(equivalentTo: Self.preferredLocale())
        else {
            throw VoiceEngineError.unavailable("On-device English speech recognition isn't available on this Mac, so voice commands are off. (Darkslide never sends audio to a server.)")
        }
        try check(run)
        // Volatile + fast results: the tracker can act on a settled partial before finalization.
        let transcriber = SpeechTranscriber(locale: locale, transcriptionOptions: [],
                                            reportingOptions: [.volatileResults, .fastResults],
                                            attributeOptions: [.transcriptionConfidence])
        try await Self.installModel(for: transcriber)
        try check(run)

        let analyzer = SpeechAnalyzer(modules: [transcriber],
                                      options: .init(priority: .userInitiated, modelRetention: .lingering))
        run.analyzer = analyzer
        let context = AnalysisContext()
        context.contextualStrings[.general] = vocabulary
        try await analyzer.setContext(context)
        try check(run)

        let audio = AVAudioEngine()
        let micFormat = audio.inputNode.outputFormat(forBus: 0)
        guard micFormat.sampleRate > 0, micFormat.channelCount > 0 else {
            throw VoiceEngineError.unavailable("No microphone is connected.")
        }
        guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber], considering: micFormat)
        else { throw VoiceEngineError.unavailable("The speech model can't read this microphone's audio.") }
        try await analyzer.prepareToAnalyze(in: format)
        try check(run)

        let (inputStream, inputContinuation) = AsyncStream<AnalyzerInput>.makeStream()
        let (events, eventContinuation) = AsyncStream<VoiceEngineEvent>.makeStream()
        run.input = inputContinuation
        run.events = eventContinuation
        run.results = Task.detached {
            do {
                for try await r in transcriber.results {
                    eventContinuation.yield(.transcript(Self.transcript(r)))
                }
            } catch is CancellationError {
            } catch {
                eventContinuation.yield(.failed("Speech recognition stopped: \(error.localizedDescription)"))
            }
            eventContinuation.finish()
        }
        try Self.installTap(on: audio.inputNode, from: micFormat, to: format, into: inputContinuation)
        run.audio = audio
        // A new input device (headset plugged in, mic unplugged) stops this engine: end the
        // session; its stream ending tells the controller to start a new one.
        let id = run.id
        run.observer = NotificationCenter.default.addObserver(forName: .AVAudioEngineConfigurationChange, object: audio,
                                                              queue: nil) { [weak self] _ in
            Task { await self?.stop(id) }
        }
        try await analyzer.start(inputSequence: inputStream)
        try check(run)
        audio.prepare()
        try audio.start()
        return VoiceSession(id: run.id, events: events)
    }

    public func finish(_ session: Int) async {
        guard let run = current, run.id == session else { return }
        current = nil
        closeMicrophone(run)
        guard let analyzer = run.analyzer else { return await close(run) }
        run.analyzer = nil
        // Bounded: a stuck model never holds up the next start.
        let done = Task { try? await analyzer.finalizeAndFinishThroughEndOfInput() }
        let timeout = Task {
            guard (try? await Task.sleep(for: .seconds(2))) != nil else { return } // finished in time
            await analyzer.cancelAndFinishNow()
        }
        _ = await done.value
        timeout.cancel()
        await run.results?.value
        run.events?.finish()
    }

    public func stop(_ session: Int) async {
        guard let run = current, run.id == session else { return }
        current = nil
        await close(run)
    }

    /// Release everything `run` holds (idempotent).
    private func close(_ run: Run) async {
        closeMicrophone(run)
        run.results?.cancel()
        run.results = nil
        if let analyzer = run.analyzer {
            run.analyzer = nil
            await analyzer.cancelAndFinishNow()
        }
        run.events?.finish()
        run.events = nil
    }

    private func closeMicrophone(_ run: Run) {
        if let observer = run.observer { NotificationCenter.default.removeObserver(observer) }
        run.observer = nil
        if let audio = run.audio {
            audio.stop()
            audio.inputNode.removeTap(onBus: 0)
        }
        run.audio = nil
        run.input?.finish()
        run.input = nil
    }

    private static func installModel(for transcriber: SpeechTranscriber) async throws {
        switch await AssetInventory.status(forModules: [transcriber]) {
        case .installed: return
        case .unsupported:
            throw VoiceEngineError.unavailable("This Mac's speech model doesn't support English, so voice commands are off.")
        case .supported, .downloading:
            // The system downloads Apple's speech model once; no audio is involved.
            if let request = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
                try await request.downloadAndInstall()
            }
        @unknown default: return
        }
    }

    /// Nonisolated: the tap runs on the audio thread.
    private nonisolated static func installTap(on node: AVAudioInputNode, from micFormat: AVAudioFormat,
                                               to format: AVAudioFormat,
                                               into input: AsyncStream<AnalyzerInput>.Continuation) throws {
        guard let converter = AVAudioConverter(from: micFormat, to: format) else {
            throw VoiceEngineError.unavailable("The microphone's audio format isn't supported.")
        }
        let box = ConverterBox(converter)
        node.installTap(onBus: 0, bufferSize: 2048, format: micFormat) { buffer, _ in
            if let converted = box.convert(buffer, to: format) { input.yield(AnalyzerInput(buffer: converted)) }
        }
    }

    private static func transcript(_ r: SpeechTranscriber.Result) -> VoiceTranscript {
        let confidences = r.text.runs.compactMap(\.transcriptionConfidence)
        return VoiceTranscript(String(r.text.characters), isFinal: r.isFinal, confidence: confidences.min())
    }
}

/// The converter, used only from the audio thread's tap.
private final class ConverterBox: @unchecked Sendable {
    private let converter: AVAudioConverter

    init(_ converter: AVAudioConverter) { self.converter = converter }

    func convert(_ buffer: AVAudioPCMBuffer, to format: AVAudioFormat) -> AVAudioPCMBuffer? {
        let ratio = format.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up)) + 1
        guard let out = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else { return nil }
        // The converter calls the block synchronously, on this thread.
        nonisolated(unsafe) var fed = false
        var error: NSError?
        let status = converter.convert(to: out, error: &error) { _, inputStatus in
            if fed {
                inputStatus.pointee = .noDataNow
                return nil
            }
            fed = true
            inputStatus.pointee = .haveData
            return buffer
        }
        return status == .error || out.frameLength == 0 ? nil : out
    }
}

/// The system's microphone and speech-recognition consent.
public struct SystemVoicePermissions: VoicePermissions {
    public init() {}

    public func microphone() -> VoicePermission {
        switch AVAudioApplication.shared.recordPermission {
        case .granted: .granted
        case .undetermined: .notDetermined
        default: .denied
        }
    }

    public func speech() -> VoicePermission {
        switch SFSpeechRecognizer.authorizationStatus() {
        case .authorized: .granted
        case .notDetermined: .notDetermined
        default: .denied
        }
    }

    public func requestMicrophone() async -> Bool {
        await AVAudioApplication.requestRecordPermission()
    }

    public func requestSpeech() async -> Bool {
        await withCheckedContinuation { c in
            SFSpeechRecognizer.requestAuthorization { c.resume(returning: $0 == .authorized) }
        }
    }
}
