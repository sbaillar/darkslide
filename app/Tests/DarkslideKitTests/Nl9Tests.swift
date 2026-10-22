import DarkslideAI
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// A model backend that over-edits like Foundation Models did in the review (H2):
/// every control moves, whatever the text says.
struct OverEditingModel: NaturalLanguageEditService {
    var intent = { () -> EditIntent in
        var i = EditIntent(temperatureShift: 1600, tintShift: -25, exposureShift: 1, contrastShift: 20, highlightsShift: -10,
                           shadowsShift: 15, saturationShift: 10, cropAspect: "4:5", straighten: true)
        i.straightenDegrees = 5
        return i
    }()

    func intent(for text: String) async throws -> EditIntent { intent }
}

struct FailingModel: NaturalLanguageEditService {
    func intent(for text: String) async throws -> EditIntent { throw NaturalLanguageEditError.modelUnavailable("off") }
}

/// Records what it was asked (to prove only leftovers reach the model).
final class RecordingModel: NaturalLanguageEditService, @unchecked Sendable {
    var asked: [String] = []
    let answer: EditIntent
    init(_ answer: EditIntent) { self.answer = answer }
    func intent(for text: String) async throws -> EditIntent {
        asked.append(text)
        return answer
    }
}

@Suite struct Nl9GuardTests {
    let guarded = GuardedEditService(models: [OverEditingModel()])

    /// The reviewer's table: nothing the text doesn't mention may move.
    @Test func reviewerPhrasesTouchOnlyWhatTheyMention() async throws {
        let contrast = try await guarded.intent(for: "more contrast")
        #expect(contrast.contrastShift > 0)
        #expect(contrast.temperatureShift == 0 && contrast.exposureShift == 0 && contrast.warmth == 0)
        #expect(contrast.saturationShift == 0 && contrast.shadowsShift == 0 && !contrast.straighten)

        let shadows = try await guarded.intent(for: "lift shadows")
        #expect(shadows.shadowsShift > 0 && shadows.exposureShift == 0 && shadows.contrastShift == 0)

        let bw = try await guarded.intent(for: "black and white")
        #expect(bw.saturationShift == -100)
        #expect(bw.contrastShift == 0 && bw.highlightsShift == 0 && bw.shadowsShift == 0 && bw.temperatureShift == 0)

        let wb = try await guarded.intent(for: "make it warmer and brighter")
        #expect(wb.warmth > 0 || wb.temperatureShift > 0)
        #expect(wb.exposureShift > 0 && wb.exposureShift <= 0.5)
        #expect(wb.contrastShift == 0 && wb.highlightsShift == 0 && wb.shadowsShift == 0 && wb.saturationShift == 0
                && wb.tintShift == 0)

        let by400 = try await guarded.intent(for: "warmer by 400")
        #expect(by400.temperatureShift == 400 || by400.warmth > 0, "\(by400)")
        #expect(by400.exposureShift == 0 && by400.tintShift == 0 && by400.contrastShift == 0)

        let seven = try await guarded.intent(for: "brighter by 7 stops")
        #expect(seven.exposureShift == 0, "never a silent clamp")
        #expect(seven.unrecognized.contains { $0.contains("7") })
        #expect(seven.temperatureShift == 0 && seven.contrastShift == 0)

        let two = try await guarded.intent(for: "straighten 2 degrees")
        #expect(two.straightenDegrees.map { abs($0) } == 2, "\(two)")
        #expect(two.exposureShift == 0 && two.temperatureShift == 0 && !two.straighten)
    }

    @Test func rulesWinAndOnlyLeftoversReachTheModel() async throws {
        let model = RecordingModel(EditIntent(exposureShift: -2, contrastShift: 90))
        let service = GuardedEditService(models: [model])
        let i = try await service.intent(for: "a bit brighter")
        #expect(i.exposureShift == 0.25 && model.asked.isEmpty, "the rules understood everything")
        let merged = IntentGuard.merge(rules: EditIntent(exposureShift: 0.5), model: EditIntent(exposureShift: -2, contrastShift: 10))
        #expect(merged.exposureShift == 0.5 && merged.contrastShift == 10)
        var r = EditIntent()
        r.straightenDegrees = 2
        var m = EditIntent(straighten: true)
        m.straightenDegrees = 9
        m.cropAspect = "1:1"
        let s = IntentGuard.merge(rules: r, model: m)
        #expect(s.straightenDegrees == 2 && !s.straighten && s.cropAspect == "1:1")
    }

    @Test func constrainMasksClampsAndReports() {
        let over = OverEditingModel().intent
        // "a bit" halves the step; unmentioned controls vanish.
        let bit = IntentGuard.constrain(over, clauses: ["make it pop a bit"])
        #expect(bit.intent.contrastShift == 10 && bit.intent.exposureShift == 0 && bit.intent.temperatureShift == 0)
        #expect(bit.unrecognized.isEmpty)
        // "a lot" doubles it; the model's smaller value is kept.
        let lot = IntentGuard.constrain(EditIntent(exposureShift: 0.3), clauses: ["way brighter please"])
        #expect(lot.intent.exposureShift == 0.3)
        let big = IntentGuard.constrain(EditIntent(exposureShift: -3), clauses: ["much darker"])
        #expect(big.intent.exposureShift == -1)
        // A clause the model left untouched (or that names nothing) is reported.
        let none = IntentGuard.constrain(EditIntent(), clauses: ["remove the power lines"])
        #expect(none.unrecognized == ["remove the power lines"] && none.intent.isEmpty)
        // Numbers are taken as given, in range; straighten and crop need their words.
        var tilt = EditIntent(straighten: true)
        tilt.straightenDegrees = -8
        let t = IntentGuard.constrain(tilt, clauses: ["tilt it 3 degrees"])
        #expect(t.intent.straightenDegrees == -3 && !t.intent.straighten)
        let level = IntentGuard.constrain(EditIntent(straighten: true), clauses: ["level the horizon"])
        #expect(level.intent.straighten)
        let crop = IntentGuard.constrain(EditIntent(cropAspect: "4:5"), clauses: ["crop for instagram"])
        #expect(crop.intent.cropAspect == "4:5")
        let tooFar = IntentGuard.constrain(EditIntent(contrastShift: 50), clauses: ["contrast +150"])
        #expect(tooFar.intent.contrastShift == 0 && tooFar.unrecognized.first?.contains("±100") == true)
    }

    @Test func mentionsIntensityAndNumbers() {
        #expect(ControlMentions.controls(in: "Warmer and brighter") == [.whiteBalance, .exposure])
        #expect(ControlMentions.controls(in: "black and white") == [.saturation])
        #expect(ControlMentions.controls(in: "level the horizon").contains(.straighten))
        #expect(ControlMentions.controls(in: "remove the power lines").isEmpty)
        #expect(ControlMentions.intensity(of: "a bit warmer") == 0.5)
        #expect(ControlMentions.intensity(of: "much warmer") == 2)
        #expect(ControlMentions.intensity(of: "warmer") == 1)
        #expect(ControlMentions.number(in: "warmer by 400") == 400)
        #expect(ControlMentions.number(in: "0,5 stops") == 0.5)
        #expect(ControlMentions.number(in: "warmer") == nil)
    }

    @Test func backendsFallThroughInOrder() async throws {
        let second = RecordingModel(EditIntent(exposureShift: 0.4))
        let s = GuardedEditService(models: [FailingModel(), second])
        let i = try await s.intent(for: "the photo is too gloomy")
        #expect(second.asked == ["the photo is too gloomy"])
        #expect(i.unrecognized == ["the photo is too gloomy"], "no control word: nothing applied, reported")
        let none = try await GuardedEditService(models: []).intent(for: "remove the power lines")
        #expect(!none.unrecognized.isEmpty && none.isEmpty)
        // Pixel-needing notes from the rules survive, and aren't sent to models.
        let auto = RecordingModel(EditIntent())
        let a = try await GuardedEditService(models: [auto]).intent(for: "auto")
        #expect(a.unrecognized.contains { $0.contains("Auto") } && auto.asked.isEmpty)
        #expect(GuardedEditService.isNote("auto (use the Auto button: it needs the image)"))
    }
}

@Suite struct Nl9JevTests {
    static func answers(_ levels: [String: Any]) -> Data {
        var a: [String: Any] = [:]
        for (k, v) in levels { a[k] = v }
        return try! JSONSerialization.data(withJSONObject: ["model": "jev-1", "answers": a])
    }

    @Test func sendsOnlyTextAndMapsLevels() async throws {
        let seen = Nl9Locked<[URLRequest]>([])
        let data = Self.answers([
            "warmth": ["type": "score", "probabilities": ["0": 0.01, "5": 0.9, "3": 0.09]],
            "exposure": ["type": "score", "score": 4.1],
            "contrast": ["type": "score", "probabilities": ["It does not ask to change the contrast": 0.8, "x": 0.2]],
            "saturation": ["type": "score", "score": 0.2],
        ])
        let jev = JevEditService(apiKey: { "k-123" }, transport: { req in
            seen.withLock { $0.append(req) }
            return (data, 200)
        })
        let i = try await jev.intent(for: "warmer and a touch brighter")
        #expect(i.temperatureShift == 500 && i.exposureShift == 0.25 && i.contrastShift == 0 && i.saturationShift == -40)
        let req = try #require(seen.withLock { $0.first })
        #expect(req.url == JevEditService.endpoint && req.httpMethod == "POST")
        #expect(req.value(forHTTPHeaderField: "Authorization") == "Bearer k-123")
        let body = try #require(req.httpBody.flatMap { try JSONSerialization.jsonObject(with: $0) as? [String: Any] })
        #expect((body["state"] as? [String: String]) == ["request": "warmer and a touch brighter"], "text only, no pixels")
        let qs = try #require(body["questions"] as? [String: [String: Any]])
        #expect(Set(qs.keys) == Set(JevEditService.questions.map(\.id)))
        #expect(qs.values.allSatisfy { ($0["type"] as? String) == "score" && ($0["criteria"] as? [String])?.count == 7 })
    }

    /// The URLSession transport, answered by a stub protocol (no network in tests).
    @Test func urlSessionTransportReturnsBodyAndStatus() async throws {
        URLProtocol.registerClass(Nl9StubProtocol.self)
        defer { URLProtocol.unregisterClass(Nl9StubProtocol.self) }
        var req = URLRequest(url: URL(string: "https://nl9.stub.invalid/v1/systemone")!)
        req.httpMethod = "POST"
        let (data, status) = try await JevEditService.urlSession(req)
        #expect(status == 418 && String(decoding: data, as: UTF8.self) == "stub")
    }

    @Test func offErrorsAndMalformed() async {
        let off = JevEditService(apiKey: { nil }, transport: { _ in Issue.record("no request when off"); return (Data(), 200) })
        await #expect(throws: JevEditService.JevError.disabled) { try await off.intent(for: "warmer") }
        let limited = JevEditService(apiKey: { "k" }, transport: { _ in (Data(), 429) })
        await #expect(throws: JevEditService.JevError.http(429)) { try await limited.intent(for: "warmer") }
        let junk = JevEditService(apiKey: { "k" }, transport: { _ in (Data("{}".utf8), 200) })
        await #expect(throws: JevEditService.JevError.malformed) { try await junk.intent(for: "warmer") }
        #expect(JevEditService.level([:], criteria: []) == nil)
    }

    @Test func settingsKeepTheKeyInTheStoreAndDefaultOff() throws {
        let defaults = UserDefaults(suiteName: testSuiteName())!
        let box = Nl9Locked<String?>(nil)
        let store = JevSettings.KeyStore(read: { box.withLock { $0 } }, write: { k in box.withLock { $0 = k }; return true })
        let jev = JevSettings(defaults: defaults, store: store)
        #expect(!jev.enabled && !jev.hasKey && jev.activeKey() == nil, "off by default")
        jev.setKey("  secret \n")
        #expect(jev.hasKey && jev.activeKey() == nil, "a key alone doesn't turn it on")
        jev.enabled = true
        #expect(jev.activeKey() == "secret")
        #expect(defaults.string(forKey: "secret") == nil && box.withLock { $0 } == "secret", "the key never goes to defaults")
        let service = makeEditService(jev: jev, foundationModels: false)
        #expect(service.models.count == 1 && service.models.first is JevEditService)
        #expect(makeEditService(jev: jev, foundationModels: true).models.count == 2)
    }

    @MainActor @Test func aiSettingsModelSavesAndRemoves() {
        let box = Nl9Locked<String?>(nil)
        let fail = Nl9Locked(false)
        let store = JevSettings.KeyStore(read: { box.withLock { $0 } },
                                         write: { k in if fail.withLock({ $0 }) { return false }; box.withLock { $0 = k }; return true })
        let m = AISettingsModel(jev: JevSettings(defaults: UserDefaults(suiteName: testSuiteName())!, store: store),
                                foundationModelsAvailable: true)
        #expect(!m.jevEnabled && !m.hasKey && m.order == "Darkslide's own phrase rules → Apple Intelligence on this Mac")
        m.saveKey()
        #expect(m.message == nil, "empty entry does nothing")
        m.keyEntry = "abc"
        m.saveKey()
        #expect(m.hasKey && m.keyEntry.isEmpty && m.message?.contains("saved") == true)
        m.jevEnabled = true
        #expect(m.order.contains("Jev"))
        m.removeKey()
        #expect(!m.hasKey && box.withLock { $0 } == nil)
        fail.withLock { $0 = true }
        m.keyEntry = "x"
        m.saveKey()
        #expect(m.message?.contains("refused") == true)
    }
}

@MainActor @Suite struct Nl9AppTests {
    /// An AI service over the real guarded parser (rules + an over-editing model).
    static func app(horizon: Double? = 3, confidence: Double = 0.9) async -> AppModel {
        let settings = makeSettings()
        settings.onboardingComplete = true
        settings.autoAdvance = false
        let ai = VisionAIService(language: GuardedEditService(models: [OverEditingModel()]))
        let m = AppModel(sessions: MockSessionStore(counts: [4]), images: MockImageProvider(latency: .zero),
                         renderer: FakeRenderer(), importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: ai, settings: settings)
        m.watchCards = false
        m.toastDuration = .seconds(60)
        m.horizonDetector = Nl9Horizon(reading: horizon.map { HorizonReading(degrees: $0, confidence: confidence) })
        await m.start()
        return m
    }

    @Test func warmerAndABitBrighterAppliesOnlyThoseAndSaysWhat() async throws {
        let m = await Self.app()
        let id = try #require(m.focusedID)
        let before = m.edit(for: id)
        let toast = try #require(await m.runInstruction("warmer and a bit brighter"))
        let after = m.edit(for: id)
        #expect(after.whiteBalance.temperature > (before.whiteBalance.asShot ? 5500 : before.whiteBalance.temperature) - 1)
        #expect(after.exposure.ev == before.exposure.ev + 0.25)
        #expect(after.exposure.contrast == before.exposure.contrast && after.hdr == before.hdr && after.crop == before.crop)
        #expect(toast.message.contains("EV") && toast.message.contains("K") && toast.message.hasSuffix("⌘Z to undo"), "\(toast.message)")
        #expect(m.history.undoLabel == "“warmer and a bit brighter”")
        // "undo" in words.
        await m.runInstruction("undo")
        #expect(m.edit(for: id) == before)
        await m.runInstruction("redo")
        #expect(m.edit(for: id) == after)
        await m.runInstruction("reset")
        #expect(m.edit(for: id) == EditState())
        #expect(await m.runInstruction("reset") == nil, "nothing to reset")
        #expect(await m.runInstruction("   ") == nil)
    }

    @Test func straightenRunsTheHorizonAndUnknownWordsAreReported() async throws {
        let m = await Self.app(horizon: 2.5)
        let id = try #require(m.focusedID)
        let toast = try #require(await m.runInstruction("level the horizon"))
        #expect(m.edit(for: id).crop.angle == -2.5)
        #expect(toast.message.contains("straighten"), "\(toast.message)")
        let flat = await Self.app(horizon: nil)
        let none = try #require(await flat.runInstruction("straighten"))
        #expect(none.message.contains("No clear horizon") && flat.edit(for: flat.focusedID!) == EditState())
        let unknown = try #require(await m.runInstruction("remove the power lines"))
        #expect(unknown.isError && unknown.message.hasPrefix("Didn't understand: “remove the power lines”"))
        let mixed = try #require(await m.runInstruction("brighter, remove the power lines"))
        #expect(!mixed.isError && mixed.message.contains("EV") && mixed.message.contains("Didn't understand"))
    }

    /// C3 through words: "crop 4:5" crops (like the aspect chip), it doesn't only record
    /// the ratio (the final-journeys run showed rect (0,0,1,1) with aspect 4:5).
    @Test func cropToAnAspectInWordsCrops() async throws {
        let m = await Self.app()
        let id = try #require(m.focusedID)
        m.viewer.setViewerSize(CGSize(width: 1200, height: 800), backingScale: 1)
        let toast = try #require(await m.runInstruction("crop 4:5"))
        let e = m.edit(for: id)
        #expect(e.crop.aspect == .r4x5 && e.crop.rect != CGRect(x: 0, y: 0, width: 1, height: 1), "\(e.crop)")
        #expect(toast.message.contains("crop"), "\(toast.message)")
    }

    /// Auto (⇧A) never straightens: only the explicit Straighten actions do.
    @Test func autoNeverStraightens() async throws {
        let m = await Self.app(horizon: 6)
        let id = try #require(m.focusedID)
        m.updateEdit(label: "tilt") { $0.crop.angle = 1.2 }
        await m.autoAdjust()
        #expect(m.edit(for: id).crop.angle == 1.2)
    }

    @Test func fallsBackToTheServiceWhenItHasNoIntents() async throws {
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: MockSessionStore(counts: [2]), images: MockImageProvider(latency: .zero),
                         renderer: FakeRenderer(), importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings)
        m.watchCards = false
        m.toastDuration = .seconds(60)
        await m.start()
        await m.applyInstruction("warmer")
        #expect(m.toast?.message.contains("⌘Z") == true || m.toast?.message == "Nothing to change")
    }

    @Test func commandBarFiltersHighlightsAndSubmits() async throws {
        let m = await Self.app()
        let bar = m.commandBar
        #expect(!bar.isPresented)
        #expect(CommandCatalog.isEnabled(.askAI, model: m) && CommandCatalog.title(.askAI, model: m) == "Edit with Words…")
        #expect(MenuCommand.askAI.shortcut == .init(key: "k", modifiers: [.command]))
        CommandCatalog.perform(.askAI, model: m)
        #expect(bar.isPresented && bar.filtered.count == 6 && bar.submission == CommandBarModel.suggestions[0])
        bar.moveHighlight(by: -1)
        #expect(bar.highlighted == 5)
        bar.moveHighlight(by: 2)
        #expect(bar.highlighted == 1)
        bar.text = "crop"
        #expect(bar.filtered == ["crop 4:5", "crop 1:1"])
        bar.text = "zzz"
        bar.highlighted = 0
        #expect(bar.filtered.isEmpty && bar.submission == "zzz")
        bar.moveHighlight(by: 1)
        bar.text = "more contrast"
        let t = try #require(await bar.submit(to: m))
        #expect(t.message.contains("contrast") && !bar.isPresented && bar.text.isEmpty && bar.lastResult == t)
        bar.present()
        bar.dismiss()
        #expect(!bar.isPresented)
        #expect(ToolbarControl.ask.control(m).label == "Edit with words")
        #expect(KeyboardRouter.cheatSheet().contains { $0.rows.contains { $0.keys == "⌘K" } })
    }

    @Test func summariesAndToasts() {
        var a = EditState()
        a.whiteBalance.asShot = false
        a.whiteBalance.temperature = 6435
        a.whiteBalance.tint = 2
        a.exposure.ev = 1
        a.exposure.saturation = -100
        a.exposure.contrast = 20
        a.hdr.highlights = 10
        a.hdr.shadows = -5
        a.crop.angle = 1.5
        let s = InstructionSummary.describe(before: EditState(), after: a, asShot: WhiteBalanceValue(kelvin: 5235, tint: 0))
        #expect(s == "5235→6435 K, +2 tint, +1.0 EV, +20 contrast, black and white, +10 highlights, −5 shadows, straighten +1.5°")
        #expect(InstructionSummary.signed(2.25, "x") == "+2.3 x" || InstructionSummary.signed(2.25, "x") == "+2.2 x")
        let t = AppModel.instructionToast(summary: "", unrecognized: [])
        #expect(t.message == "Nothing to change")
        #expect(InstructionCommand("Undo that.") == .undo && InstructionCommand("Start over") == .reset
                && InstructionCommand("redo") == .redo && InstructionCommand("warmer") == nil)
    }
}

struct Nl9Horizon: HorizonDetecting {
    var reading: HorizonReading?
    func horizon(of photo: Photo, images: any ImageProvider) async throws -> HorizonReading? { reading }
}

// MARK: - AI Cull (M8) and batched analysis

@MainActor @Suite struct Nl9CullTests {
    @Test func bestOfAGroupIsNeverRejected() {
        func rec(_ s: PickSuggestionKind, score: Float, hard: Bool = false) -> CullResultRecord {
            CullResultRecord(id: 1, score: score, sharpness: 0.5, exposure: 0.5, noise: nil,
                             reasons: hard ? [ReasonRecord(kind: "blink", text: "eyes closed", hard: true, negative: true)] : [],
                             group: 0, suggestion: s)
        }
        #expect(DSAI.suggestion(for: rec(.best, score: 0.1), groupSize: 5).pick == .none)
        #expect(DSAI.suggestion(for: rec(.best, score: 0.9, hard: true), groupSize: 5).pick == .none)
        #expect(DSAI.suggestion(for: rec(.best, score: 0.9), groupSize: 5).pick == .picked)
        #expect(DSAI.suggestion(for: rec(.reject, score: 0.9), groupSize: 5).pick == .rejected)
        // A lone frame (no burst) is still judged on its own: soft or failing → rejected.
        #expect(DSAI.suggestion(for: rec(.best, score: 0.1), groupSize: 1).pick == .rejected)
        #expect(DSAI.suggestion(for: rec(.best, score: 0.8), groupSize: 1).pick == .picked)
    }

    @Test func onlyUndecidedPhotosChangeAndTheToastCountsChanges() {
        var photos = (1...5).map { samplePhoto(Int64($0)) }
        photos[1].rating = 3
        photos[2].colorTag = .red
        photos[3].pick = .picked
        let sug = Dictionary(uniqueKeysWithValues: photos.map {
            ($0.id, CullSuggestion(photoID: $0.id, score: 0.1, pick: .rejected, reasons: []))
        })
        let plan = AICullApplication.plan(photos, suggestions: sug)
        #expect(plan.changes == [photos[0].id: .rejected, photos[4].id: .rejected])
        #expect(plan.message == "AI Cull: 0 picks, 2 rejects; 3 already rated, tagged or flagged left alone. ⌘Z to undo")
        let none = AICullApplication.plan(photos, suggestions: [:])
        #expect(none.changes.isEmpty && none.message == "AI Cull: no changes")
        let one = AICullApplication.plan([photos[0]], suggestions: [photos[0].id: CullSuggestion(
            photoID: photos[0].id, score: 0.9, pick: .picked, reasons: [])])
        #expect(one.message == "AI Cull: 1 pick, 0 rejects. ⌘Z to undo")
    }

    @Test func runAICullLeavesRatedPhotosAlone() async throws {
        let m = makeModel(counts: [30])
        await m.start()
        let ids = m.visibleIDs
        m.focus(ids[0], extend: false)
        m.setRating(4)
        let before = m.photo(ids[0])!.pick
        await m.runAICull()
        #expect(m.photo(ids[0])!.pick == before, "rated: untouched")
        #expect(m.toast?.message.hasPrefix("AI Cull:") == true)
    }

    /// Batched analysis (`add_batch`, GPU) gives the same suggestions as one `add` per preview.
    @Test func batchedCullMatchesSingleAdds() async throws {
        let photos = (1...20).map { i -> Photo in
            var p = samplePhoto(Int64(i))
            p.meta.captureDate = Date(timeIntervalSince1970: 1_790_000_000 + Double(i / 4))
            return p
        }
        let images = MockImageProvider(latency: .zero)
        let single = try await VisionAIService(batchSize: 1).cull(photos, images: images) { _, _ in }
        let seen = Nl9Locked<[Int]>([])
        let batched = try await VisionAIService(maxConcurrent: 8, batchSize: 4).cull(photos, images: images) { d, _ in
            seen.withLock { $0.append(d) }
        }
        #expect(single == batched)
        // Progress per batch, never more than `batchSize` at a time (bounded FFI buffers).
        let steps = seen.withLock { $0 }
        #expect(steps.last == 20 && steps.count == 5)
        #expect(zip(steps, [0] + steps).allSatisfy { $0 - $1 == 4 }, "\(steps)")
    }
}

final class Nl9Locked<T>: @unchecked Sendable {
    private var value: T
    private let lock = NSLock()
    init(_ v: T) { value = v }
    func withLock<R>(_ f: (inout T) -> R) -> R { lock.withLock { f(&value) } }
}
