import LuxtableFFI
import Foundation
import Testing
@testable import LuxtableCore

/// AI Cull's reject note: why a photo was rejected, shown while the AI's reject
/// stands and saved in the sidecar (`cull_note`).
@MainActor @Suite struct CullNoteTests {
    private func record(_ s: PickSuggestionKind, score: Float, note: String) -> CullResultRecord {
        CullResultRecord(id: 1, score: score, sharpness: score, exposure: 1, noise: nil, reasons: [], group: 0,
                         suggestion: s, rejectReasons: [], rejectNote: note)
    }

    @Test func onlyARejectSuggestionCarriesTheNote() {
        let note = "Out of focus (sharpness 0.12, best in group 0.71)"
        let reject = DSAI.suggestion(for: record(.reject, score: 0.2, note: note), groupSize: 3)
        #expect(reject.pick == .rejected && reject.rejectNote == note)
        #expect(DSAI.suggestion(for: record(.best, score: 0.9, note: note), groupSize: 3).rejectNote == nil)
        #expect(DSAI.suggestion(for: record(.keep, score: 0.6, note: note), groupSize: 3).rejectNote == nil)
        #expect(DSAI.suggestion(for: record(.reject, score: 0.2, note: ""), groupSize: 3).rejectNote == nil)
    }

    @Test func aNewFlagClearsTheNote() {
        var p = samplePhoto()
        #expect(AICullApplication.rejectNote(p) == nil && AICullApplication.rejectHelp(p) == "Rejected")
        AICullApplication.apply(.rejected, note: "Eyes closed", to: &p)
        #expect(p.pick == .rejected && p.cullNote == "Eyes closed")
        #expect(AICullApplication.rejectNote(p) == "Eyes closed")
        #expect(AICullApplication.rejectHelp(p) == "Rejected by AI Cull: Eyes closed")
        p.pick = .rejected // the same flag again: no change
        #expect(p.cullNote == "Eyes closed")
        p.rating = 3
        #expect(AICullApplication.rejectNote(p) == "Eyes closed", "other changes keep it")
        p.pick = .picked
        #expect(p.cullNote == nil && AICullApplication.rejectNote(p) == nil)
        AICullApplication.apply(.picked, note: "ignored", to: &p)
        #expect(p.cullNote == nil, "a pick has no reject note")
        #expect(AICullApplication.rejectNote(nil) == nil)
    }

    @Test func writePlanSendsTheNoteAfterTheFlag() {
        let a = samplePhoto(1)
        var rejected = a
        AICullApplication.apply(.rejected, note: "Eyes closed", to: &rejected)
        // The AI's reject: the flag and the note.
        var plan = RustSessionStore.writePlan([rejected], stored: [a.id: a])
        #expect(plan.picks[.rejected] == [1] && plan.cullNotes["Eyes closed"] == [1])
        // A new flag by hand: only the flag (Rust clears the note with it).
        var picked = rejected
        picked.pick = .picked
        plan = RustSessionStore.writePlan([picked], stored: [a.id: rejected])
        #expect(plan.picks[.picked] == [1] && plan.cullNotes.isEmpty)
        // Undo: the reject and its note come back.
        plan = RustSessionStore.writePlan([rejected], stored: [a.id: picked])
        #expect(plan.picks[.rejected] == [1] && plan.cullNotes["Eyes closed"] == [1])
        // The same flag, the note cleared: the note alone.
        var cleared = rejected
        cleared.cullNote = nil
        plan = RustSessionStore.writePlan([cleared], stored: [a.id: rejected])
        #expect(plan.picks.isEmpty && plan.cullNotes[nil] == [1])
        #expect(RustSessionStore.writePlan([rejected], stored: [a.id: rejected]) == RustSessionStore.WritePlan())
    }

    @Test func recordMappingKeepsTheNoteOnlyOnARejectedPhoto() {
        var r = VariantRecord(variantId: 1, imageId: 1, originalPath: "/x/A.ARW", fileName: "A.ARW", name: "", rating: 0,
                              colorTag: .none, pick: .rejected, edited: false, readOnly: false, missing: false,
                              fileChanged: false, keptInStore: false, fileSize: 1, fileSequence: nil, captureTimeMs: nil,
                              width: 6000, height: 4000, meta: nil, cullNote: "Eyes closed")
        #expect(RustSessionStore.photo(from: r).cullNote == "Eyes closed")
        r.pick = .unflagged
        #expect(RustSessionStore.photo(from: r).cullNote == nil)
    }

    /// AI Cull writes the note with each reject; undo takes both back; a new flag drops it.
    @Test func aiCullNotesFollowTheRejects() async throws {
        let m = makeModel(counts: [60])
        await m.start()
        await m.runAICull()
        let rejected = m.visibleIDs.filter { m.photo($0)?.pick == .rejected }
        let id = try #require(rejected.first, "the mock rejects low scores")
        #expect(rejected.allSatisfy { m.photo($0)?.cullNote == "Soft focus" })
        #expect(m.visibleIDs.filter { m.photo($0)?.pick != .rejected }.allSatisfy { m.photo($0)?.cullNote == nil })
        m.focus(id, extend: false)
        m.setPick(.picked)
        #expect(m.photo(id)?.cullNote == nil, "the user decided: the AI note no longer applies")
        m.undo()
        #expect(m.photo(id)?.pick == .rejected && m.photo(id)?.cullNote == "Soft focus", "undo restores it")
        m.undo()
        #expect(m.photo(id)?.pick == PickState.none && m.photo(id)?.cullNote == nil, "AI Cull undone")
    }

    /// Live: the note is saved in the sidecar, survives a relaunch, and a new flag clears it.
    @Test func notesPersistAcrossRelaunchLive() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return } // lt-session's tests cover it without samples
        defer { try? FileManager.default.removeItem(at: dir) }
        let defaults = testDefaults()
        let base = dir.appendingPathComponent("stores")
        let store = RustSessionStore(recents: RecentFolders(defaults: defaults), storeBase: base)
        let info = try await store.add(folder: dir)
        var p = try #require(try await store.photos(in: info).first)
        AICullApplication.apply(.rejected, note: "Duplicate of B.ARW — that frame is sharper (score 0.40 vs 0.90)", to: &p)
        try await store.save(photos: [p])
        func relaunched() async throws -> Photo {
            let again = RustSessionStore(recents: RecentFolders(defaults: defaults), storeBase: base)
            let s = try #require(try await again.listSessions().first)
            return try #require(try await again.photos(in: s).first)
        }
        #expect(try await relaunched().cullNote == p.cullNote)
        p.pick = .none
        try await store.save(photos: [p])
        let after = try await relaunched()
        #expect(after.pick == PickState.none && after.cullNote == nil)
    }
}
