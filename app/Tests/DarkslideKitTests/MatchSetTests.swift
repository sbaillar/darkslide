import CoreGraphics
import Foundation
import Observation
import Testing
@testable import DarkslideKit

/// Develop tools that can match a set: records the requests, answers from tables.
final class FakeMatchTools: DevelopTools, SetMatching, @unchecked Sendable {
    private let lock = NSLock()
    var exposure: [PhotoID: ExposureMatchOutcome] = [:]
    var whiteBalance: [PhotoID: WhiteBalanceMatchOutcome] = [:]
    var error: (any Error)?
    /// Awaited before answering (a match still running).
    var hold: (@Sendable () async -> Void)?
    private var _requests: [(reference: MatchItem, targets: [MatchItem])] = []
    var requests: [(reference: MatchItem, targets: [MatchItem])] { lock.withLock { _requests } }

    func matchExposures(reference: MatchItem, targets: [MatchItem]) async throws -> [ExposureMatchOutcome] {
        lock.withLock { _requests.append((reference, targets)) }
        await hold?()
        if let error { throw error }
        return targets.map { exposure[$0.photo.id] ?? ExposureMatchOutcome(deltaEV: 0, source: .unmatched) }
    }

    func matchWhiteBalance(reference: MatchItem, targets: [MatchItem]) async throws -> [WhiteBalanceMatchOutcome] {
        lock.withLock { _requests.append((reference, targets)) }
        await hold?()
        if let error { throw error }
        return targets.map { whiteBalance[$0.photo.id] ?? WhiteBalanceMatchOutcome(value: nil) }
    }

    func info(for photo: Photo) async throws -> PhotoDevelopInfo { throw DevelopToolsError.unavailable("fake") }
    func autoWhiteBalance(for photo: Photo) async throws -> WhiteBalanceValue { throw DevelopToolsError.unavailable("fake") }
    func autoTone(for photo: Photo, edit: EditState) async throws -> EditState { edit }
    func pickWhiteBalance(for photo: Photo, edit: EditState, at point: CGPoint, sample: Int) async throws -> WhiteBalanceValue {
        throw DevelopToolsError.unavailable("fake")
    }

    func editedThumbnail(for photo: Photo, edit: EditState) async throws -> (image: CGImage, cached: Bool) {
        throw DevelopToolsError.unavailable("fake")
    }
}

@MainActor @Suite struct MatchSetTests {
    /// A model with `tools`, the first photo focused and the first four selected.
    func model(_ tools: (any DevelopTools)? = FakeMatchTools()) async -> (AppModel, [PhotoID]) {
        let m = makeModel(counts: [12])
        m.tools = tools
        await m.start()
        let ids = Array(m.visibleIDs.filter { m.photo($0)?.isVideo == false }.prefix(4))
        m.focus(ids[0], extend: false)
        m.selection = Set(ids)
        return (m, ids)
    }

    @Test func matchTotalExposuresIsOneRelativeUndoStep() async {
        let tools = FakeMatchTools()
        let (m, ids) = await model(tools)
        m.updateEdits([ids[1]], label: "setup") { _, e in e.exposure.ev = 0.5 }
        m.updateEdits([ids[3]], label: "setup") { _, e in e.exposure.ev = 4.8 }
        m.updateEdits([ids[0]], label: "setup") { _, e in e.exposure.ev = 0.3 }
        let before = ids.map { m.edit(for: $0) }
        let undoDepth = m.history.undoStack.count
        tools.exposure = [
            ids[1]: ExposureMatchOutcome(deltaEV: 1.3, source: .metered),
            ids[2]: ExposureMatchOutcome(deltaEV: -0.704, source: .exif),
            ids[3]: ExposureMatchOutcome(deltaEV: 0.9, source: .metered), // 4.8 + 0.9 → clamped at 5
        ]
        await m.matchTotalExposures()?.value
        // The reference is the focused photo with its edit; the others are targets.
        let req = tools.requests.last!
        #expect(req.reference.photo.id == ids[0] && req.reference.edit.exposure.ev == 0.3)
        #expect(req.targets.map(\.photo.id) == Array(ids.dropFirst()))
        #expect(abs(m.edit(for: ids[1]).exposure.ev - 1.8) < 1e-9, "relative: 0.5 + 1.3")
        #expect(abs(m.edit(for: ids[2]).exposure.ev - -0.7) < 1e-9, "rounded to the slider's 0.01")
        #expect(m.edit(for: ids[3]).exposure.ev == 5)
        #expect(m.edit(for: ids[0]) == before[0], "the reference is untouched")
        #expect(m.history.undoStack.count == undoDepth + 1)
        #expect(m.history.undoLabel == "Match Total Exposures")
        #expect(m.toast?.message == "Matched the exposure of 3 photos (1 from camera settings)")
        // One undo restores every target.
        m.undo()
        #expect(ids.map { m.edit(for: $0) } == before)
        m.redo()
        #expect(abs(m.edit(for: ids[1]).exposure.ev - 1.8) < 1e-9 && m.edit(for: ids[3]).exposure.ev == 5)
    }

    @Test func unmatchedTargetsAreLeftAlone() async {
        let tools = FakeMatchTools()
        let (m, ids) = await model(tools)
        tools.exposure = [ids[1]: ExposureMatchOutcome(deltaEV: 0.4, source: .metered)]
        await m.matchTotalExposures()?.value
        #expect(abs(m.edit(for: ids[1]).exposure.ev - 0.4) < 1e-9)
        #expect(m.edit(for: ids[2]) == EditState() && m.edit(for: ids[3]) == EditState())
        #expect(m.toast?.message == "Matched the exposure of 1 photo; 2 photos could not be measured")
        // Nothing matched: no undo step, the toast says so.
        tools.exposure = [:]
        let depth = m.history.undoStack.count
        await m.matchTotalExposures()?.value
        #expect(m.history.undoStack.count == depth && m.toast?.message == "No exposures could be matched")
    }

    @Test func matchWhiteBalanceIsOneUndoStep() async {
        let tools = FakeMatchTools()
        let (m, ids) = await model(tools)
        let ref = WhiteBalanceValue(kelvin: 5612.4, tint: 3.26)
        tools.whiteBalance = [
            ids[1]: WhiteBalanceMatchOutcome(value: ref),
            ids[2]: WhiteBalanceMatchOutcome(value: ref),
            ids[3]: WhiteBalanceMatchOutcome(value: nil, error: "unreadable"),
        ]
        let depth = m.history.undoStack.count
        await m.matchWhiteBalanceToFocused()?.value
        for id in ids[1...2] {
            let wb = m.edit(for: id).whiteBalance
            #expect(!wb.asShot && wb.temperature == 5612 && wb.tint == 3.3, "\(wb)")
        }
        #expect(m.edit(for: ids[3]) == EditState())
        #expect(m.history.undoStack.count == depth + 1 && m.history.undoLabel == "Match White Balance")
        #expect(m.toast?.message == "Matched the white balance of 2 photos; 1 photo could not be read")
        m.undo()
        #expect(ids.allSatisfy { m.edit(for: $0) == EditState() })
        // Already matching: nothing to undo.
        tools.whiteBalance = [:]
        await m.matchWhiteBalanceToFocused()?.value
        #expect(m.history.undoStack.count == depth && m.toast?.message == "White balance already matches")
    }

    @Test func aSecondMatchWaitsForTheFirst() async {
        let tools = FakeMatchTools()
        let (m, ids) = await model(tools)
        tools.exposure = [ids[1]: ExposureMatchOutcome(deltaEV: 1, source: .metered)]
        let first = m.matchTotalExposures()
        #expect(first != nil && m.isMatchingSet && !m.canMatchSet)
        #expect(m.matchTotalExposures() == nil && m.matchWhiteBalanceToFocused() == nil, "deltas would add twice")
        await first?.value
        #expect(!m.isMatchingSet && m.canMatchSet)
        #expect(m.edit(for: ids[1]).exposure.ev == 1 && tools.requests.count == 1)
    }

    /// M1: a match that finishes after another session opened changes nothing in either
    /// session (the run is cancelled, and its results are dropped).
    @Test func aMatchLandingAfterASessionSwitchIsDropped() async throws {
        let tools = FakeMatchTools()
        let m = makeModel(counts: [12, 12])
        m.tools = tools
        await m.start()
        let first = try #require(m.currentSession)
        let ids = Array(m.visibleIDs.filter { m.photo($0)?.isVideo == false }.prefix(3))
        m.focus(ids[0], extend: false)
        m.selection = Set(ids)
        tools.exposure = Dictionary(uniqueKeysWithValues: ids.map { ($0, ExposureMatchOutcome(deltaEV: 1, source: .metered)) })
        tools.whiteBalance = Dictionary(uniqueKeysWithValues: ids.map {
            ($0, WhiteBalanceMatchOutcome(value: WhiteBalanceValue(kelvin: 3000, tint: 0)))
        })
        for run in [m.matchTotalExposures, m.matchWhiteBalanceToFocused] {
            // Held until cancelled (a cancelled stream ends, so one per run).
            let (stream, release) = AsyncStream<Void>.makeStream()
            defer { release.finish() }
            tools.hold = { for await _ in stream { return } }
            let task = try #require(run())
            #expect(m.sessionBoundTasks.count == 1)
            await m.open(try #require(m.sessionList.first { $0.id != first.id }))
            #expect(task.isCancelled && m.sessionBoundTasks.isEmpty)
            await task.value
            #expect(ids.allSatisfy { m.edit(for: $0) == EditState() } && m.toast?.message.hasPrefix("Matched") != true)
            #expect(m.photos.allSatisfy { m.edit(for: $0.id) == EditState() })
            await m.open(first)
            #expect(ids.allSatisfy { m.edit(for: $0) == EditState() })
            m.focus(ids[0], extend: false)
            m.selection = Set(ids)
        }
        // Still on the session it started in: applied, and the run is forgotten.
        tools.hold = nil
        await m.matchTotalExposures()?.value
        #expect(m.edit(for: ids[1]).exposure.ev == 1 && m.sessionBoundTasks.isEmpty)
    }

    @Test func failuresAreShownAndChangeNothing() async {
        let tools = FakeMatchTools()
        let (m, ids) = await model(tools)
        tools.error = DevelopToolsError.unavailable("no GPU")
        await m.matchTotalExposures()?.value
        #expect(m.toast?.isError == true)
        m.toast = nil
        await m.matchWhiteBalanceToFocused()?.value
        #expect(m.toast?.isError == true)
        // A cancelled match is silent.
        m.toast = nil
        tools.error = CancellationError()
        await m.matchTotalExposures()?.value
        await m.matchWhiteBalanceToFocused()?.value
        #expect(m.toast == nil)
        #expect(ids.allSatisfy { m.edit(for: $0) == EditState() })
    }

    @Test func commandsNeedTheToolsAReferenceAndATarget() async {
        let (m, ids) = await model()
        #expect(m.canMatchSet && m.matchTargets == Array(ids.dropFirst()))
        #expect(CommandCatalog.isEnabled(.matchExposures, model: m) && CommandCatalog.isEnabled(.matchWhiteBalance, model: m))
        // Only the focused photo selected: nothing to match.
        m.selection = [ids[0]]
        #expect(!m.canMatchSet && m.matchTotalExposures() == nil && m.matchWhiteBalanceToFocused() == nil)
        #expect(!CommandCatalog.isEnabled(.matchExposures, model: m))
        // No focus: no reference.
        m.focus(nil, extend: false)
        m.selection = Set(ids)
        #expect(m.matchTargets.isEmpty)
        // Demo mode (no tools that can match).
        let (demo, _) = await model(nil)
        #expect(!demo.canMatchSet && demo.matchTotalExposures() == nil)
        let (plain, _) = await model(FakeDevelopTools())
        #expect(!plain.canMatchSet)
    }

    /// Menu enablement observes the in-flight state: it changes when a match starts and
    /// again when it lands.
    @Test func enablementIsObservedAcrossAMatch() async {
        let tools = FakeMatchTools()
        let (m, ids) = await model(tools)
        tools.exposure = [ids[1]: ExposureMatchOutcome(deltaEV: 0.5, source: .metered)]
        final class Count: @unchecked Sendable { var n = 0 }
        let changes = Count()
        withObservationTracking { _ = m.canMatchSet } onChange: { changes.n += 1 }
        let task = m.matchTotalExposures()
        #expect(changes.n == 1 && !m.canMatchSet)
        withObservationTracking { _ = m.canMatchSet } onChange: { changes.n += 1 }
        await task?.value
        #expect(changes.n == 2 && m.canMatchSet)
    }

    @Test func videosAreNeitherReferenceNorTarget() async {
        let m = makeModel(counts: [40])
        m.tools = FakeMatchTools()
        await m.start()
        let videos = m.visibleIDs.filter { m.photo($0)?.isVideo == true }
        let stills = m.visibleIDs.filter { m.photo($0)?.isVideo == false }
        guard let v = videos.first else { return }
        m.focus(stills[0], extend: false)
        m.selection = [stills[0], stills[1], v]
        #expect(m.matchTargets == [stills[1]])
        m.focus(v, extend: false)
        m.selection = [v, stills[0]]
        #expect(m.matchTargets.isEmpty)
    }

    @Test func menuCommands() async {
        let tools = FakeMatchTools()
        let (m, ids) = await model(tools)
        #expect(MenuCommand.matchExposures.shortcut == .init(key: "m", modifiers: [.command, .option, .shift]))
        #expect(MenuCommand.matchWhiteBalance.shortcut == nil)
        #expect(CommandCatalog.title(.matchExposures, model: m) == "Match Total Exposures")
        #expect(CommandCatalog.title(.matchWhiteBalance, model: m) == "Match White Balance to Focused Photo")
        #expect(CommandCatalog.matchMenu == [.matchExposures, .matchWhiteBalance])
        tools.exposure = [ids[1]: ExposureMatchOutcome(deltaEV: 1, source: .metered)]
        tools.whiteBalance = [ids[2]: WhiteBalanceMatchOutcome(value: WhiteBalanceValue(kelvin: 4000, tint: 0))]
        #expect(CommandCatalog.perform(.matchExposures, model: m) == .none)
        #expect(await waitUntil { m.edit(for: ids[1]).exposure.ev == 1 })
        CommandCatalog.perform(.matchWhiteBalance, model: m)
        #expect(await waitUntil { m.edit(for: ids[2]).whiteBalance.temperature == 4000 })
        #expect(KeyboardRouter.cheatSheet().first { $0.section == "Edit" }!.rows.contains { $0.keys == "⌥⇧⌘M" })
    }

    @Test func summaries() {
        let metered = ExposureMatchOutcome(deltaEV: 1, source: .metered)
        #expect(MatchSummary.exposure([metered]) == "Matched the exposure of 1 photo")
        #expect(MatchSummary.exposure([]) == "No exposures could be matched")
        let odd = ExposureMatchOutcome(deltaEV: 3.5, source: .metered, exifDisagrees: true)
        #expect(MatchSummary.exposure([metered, odd, odd]) == "Matched the exposure of 3 photos; "
            + "camera settings disagreed by more than 3 EV for 2 photos (another scene?)")
        #expect(MatchSummary.whiteBalance([WhiteBalanceMatchOutcome(value: nil, error: "x")])
            == "White balance already matches; 1 photo could not be read")
    }
}

/// The live tools through ds-ffi (real samples when present).
@Suite struct RustMatchToolsTests {
    func photo(_ id: Int64, _ url: URL?) -> Photo {
        var p = samplePhoto(id)
        p.url = url
        return p
    }

    @Test func photosWithoutFilesFail() async {
        let tools = RustDevelopTools(thumbnailCache: nil)
        let none = MatchItem(photo: photo(1, nil), edit: EditState())
        await #expect(throws: DevelopToolsError.self) { try await tools.matchExposures(reference: none, targets: [none]) }
        await #expect(throws: DevelopToolsError.self) { try await tools.matchWhiteBalance(reference: none, targets: []) }
        // A file that isn't a RAW: the reference can't be read.
        let junk = FileManager.default.temporaryDirectory.appendingPathComponent("match-\(UUID()).ARW")
        try? Data("not a raw".utf8).write(to: junk)
        defer { try? FileManager.default.removeItem(at: junk) }
        let bad = MatchItem(photo: photo(2, junk), edit: EditState())
        await #expect(throws: RenderError.self) { try await tools.matchExposures(reference: bad, targets: []) }
    }

    @Test func samplesMatchThemselvesAndTheReferenceWhiteBalance() async throws {
        let files = sampleARWs()
        guard files.count >= 2 else { return }
        let tools = RustDevelopTools(thumbnailCache: nil)
        let ref = MatchItem(photo: photo(1, files[0]), edit: EditState())
        var darker = MatchItem(photo: photo(1, files[0]), edit: EditState())
        darker.edit.exposure.ev = -1
        let other = MatchItem(photo: photo(2, files[1]), edit: EditState())
        let out = try await tools.matchExposures(reference: ref, targets: [darker, other])
        #expect(out.count == 2 && out[0].source == .metered && out[0].error == nil)
        #expect(abs(out[0].deltaEV - 1) <= 0.1, "\(out[0])")
        #expect(out[1].source != .unmatched)

        // WB: a custom reference lands on every target; a target at the reference's
        // values already matches.
        var warm = ref
        warm.edit.whiteBalance = .custom(WhiteBalanceValue(kelvin: 4321, tint: -5))
        let wb = try await tools.matchWhiteBalance(reference: warm, targets: [other, warm])
        #expect(wb[0].value == WhiteBalanceValue(kelvin: 4321, tint: -5))
        #expect(wb[1] == WhiteBalanceMatchOutcome(value: nil, error: nil))
    }
}
