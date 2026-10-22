import CoreGraphics
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// In-memory `FindBackend`: records what it was asked, returns canned results.
final class FakeFindBackend: FindBackend, @unchecked Sendable {
    struct State {
        var prompts = FindPrompts(text: nil, places: [])
        var results = FindResults(query: FindQueryRecord(people: [], places: [], date: nil, minRating: nil, maxRating: nil,
                                                         flag: nil, text: ""),
                                  sessions: [], photos: [], total: 0)
        var asked: [(String, FindDay, String, [Float]?, [FindPlaceVector])] = []
        var fail: Error?
    }

    let state = SearchLocked(State())

    func prompts(_ text: String, today: FindDay, model: String) async throws -> FindPrompts {
        try state.withLock { s in
            if let e = s.fail { throw e }
            return s.prompts
        }
    }

    func find(_ text: String, today: FindDay, model: String, textVector: [Float]?, placeVectors: [FindPlaceVector],
              options: FindOptionsRecord) async throws -> FindResults {
        state.withLock { s in
            s.asked.append((text, today, model, textVector, placeVectors))
            return s.results
        }
    }
}

func findQuery(people: [FindPersonRecord] = [], places: [FindPlaceRecord] = [], date: FindDateRecord? = nil,
               min: UInt8? = nil, max: UInt8? = nil, flag: PickFlag? = nil, text: String = "") -> FindQueryRecord {
    FindQueryRecord(people: people, places: places, date: date, minRating: min, maxRating: max, flag: flag, text: text)
}

@MainActor
@Suite(.serialized) struct FindTests {
    func model(_ b: FakeFindBackend, encoder: Bool = true) async throws -> AppModel {
        let m = makeModel(counts: [6, 3])
        m.find.configure(backend: b)
        m.find.today = { Date(timeIntervalSince1970: 1_791_000_000) } // 2026-10-03 (UTC)
        if encoder {
            m.search.configure(backend: FakeSearchBackend(), models: try installedModels())
            m.search.makeEncoder = { _ in FakeEncoder() }
        }
        await m.start()
        return m
    }

    @Test func searchesWithVectorsAndExplainsTheQuery() async throws {
        let b = FakeFindBackend()
        let m = try await model(b)
        #expect(m.find.isAvailable)
        #expect(CommandCatalog.isEnabled(.find, model: m))
        CommandCatalog.perform(.find, model: m)
        #expect(m.find.isPresented)
        #expect(MenuCommand.find.shortcut == .init(key: "f", modifiers: [.command]))
        b.state.withLock { s in
            s.prompts = FindPrompts(text: "waterfall", places: ["iceland"])
            s.results.query = findQuery(
                people: [FindPersonRecord(term: "alex", names: ["Alex", "Alexander"]),
                         FindPersonRecord(term: "zephyr", names: [])],
                places: [FindPlaceRecord(term: "iceland", sources: [.looksLike, .sessionName(name: "2024-07 Iceland")]),
                         FindPlaceRecord(term: "atlantis", sources: [])],
                date: FindDateRecord(phrase: "last summer", fromMs: 1, toMs: 2, months: 0),
                min: 4, flag: .picked, text: "waterfall")
        }
        m.find.query = "  pics of alex and zephyr at a waterfall in iceland last summer  "
        await m.find.run()
        #expect(m.find.phase == .results)
        let asked = try #require(b.state.get.asked.last)
        #expect(asked.0 == "pics of alex and zephyr at a waterfall in iceland last summer")
        #expect(asked.1 == FindDay(year: 2026, month: 10, day: 3))
        #expect(asked.2 == "fake-clip")
        #expect(asked.3 == FakeEncoder.vector(PhotoSearch.prompt("waterfall")))
        #expect(asked.4.map(\.term) == ["iceland"])
        #expect(m.find.summary.map(\.text) == ["Alex or Alexander", "matched session name “2024-07 Iceland”", "atlantis",
                                                "last summer", "★★★★ or more", "Picked", "looks like: waterfall"])
        #expect(m.find.notes == ["No one named “Zephyr” yet — name faces in People.", "Nothing is called “atlantis”."])
        m.find.clear()
        #expect(m.find.results == nil && m.find.phase == .idle && m.find.summary.isEmpty && m.find.notes.isEmpty)
        m.find.dismiss()
        #expect(!m.find.isPresented)
    }

    @Test func withoutTheSearchModelPeopleAndPlacesStillWork() async throws {
        let b = FakeFindBackend()
        let m = try await model(b, encoder: false)
        b.state.withLock { $0.prompts = FindPrompts(text: "beach", places: []) }
        m.find.query = "alex at the beach"
        m.find.submit()
        await m.find.waitForSearch()
        let asked = try #require(b.state.get.asked.last)
        #expect(asked.3 == nil && asked.4.isEmpty)
        #expect(asked.2 == ClipModelDescriptor.mobileCLIPS2.id)
        #expect(m.find.visualUnavailable)
        #expect(m.find.notes.last?.contains("Download the search model") == true)
        // An empty query clears; a failure is shown.
        m.find.query = "   "
        await m.find.run()
        #expect(m.find.phase == .idle)
        b.state.withLock { $0.fail = SearchBackendError.sessionNotOpen }
        m.find.query = "x"
        await m.find.run()
        #expect(m.find.phase == .failed(SearchBackendError.sessionNotOpen.errorDescription!))
        let unconfigured = FindModel()
        unconfigured.present()
        #expect(!unconfigured.isPresented && !unconfigured.isAvailable)
    }

    @Test func wording() {
        #expect(FindModel.list(["A"]) == "A" && FindModel.list(["A", "B", "C"]) == "A, B or C" && FindModel.list([]) == "")
        #expect(FindModel.rating(min: nil, max: nil) == nil)
        #expect(FindModel.rating(min: 0, max: 0) == "Unrated")
        #expect(FindModel.rating(min: 5, max: 5) == "★★★★★")
        #expect(FindModel.rating(min: nil, max: 2) == "up to ★★")
        #expect(FindModel.flag(.rejected) == "Rejected" && FindModel.flag(.unflagged) == "Unflagged")
        let src = { (s: PlaceSourceRecord) in FindModel.source(s, term: "sicily").text }
        #expect(src(.location(value: "Vik")) == "location “Vik”")
        #expect(src(.folder(name: "2024-03 Sicily")) == "matched folder name “2024-03 Sicily”")
        #expect(src(.keyword(keyword: "Sicily")) == "keyword “Sicily”")
        #expect(src(.looksLike) == "looks like: sicily")
        #expect(FindModel.placeChip(FindPlaceRecord(term: "x", sources: [.looksLike])).kind == .looksLike)
        let photo = FindPhotoRecord(sessionRoot: "/s", sessionName: "S", path: "/s/a.ARW", score: 0.3, captureTimeMs: nil,
                                    reasons: [.person(name: "Alex", suggested: false), .person(name: "Sam", suggested: true),
                                              .place(term: "iceland", source: .looksLike), .date, .rating(stars: 0),
                                              .rating(stars: 3), .flag(pick: .picked), .visual(score: 0.274),
                                              .text(term: "zephyr"), .title(text: "Sea Cliffs")])
        #expect(FindModel.chips(photo, phrase: "2024").map(\.text) == [
            "Alex", "Sam?", "looks like: iceland", "2024", "Unrated", "★★★", "Picked", "looks like it (27)",
            "“zephyr” in folder or keywords", "titled “Sea Cliffs”",
        ])
        #expect(FindModel.chips(photo, phrase: nil)[3].text == "date")
        let s = { (r: SessionReasonRecord, n: UInt64) in
            FindModel.reason(FindSessionRecord(root: "/s", name: "S", available: true, reason: r, hits: n, bestScore: 0))
        }
        #expect(s(.name(term: "iceland"), 1) == "name matches “iceland” · 1 photo")
        #expect(s(.folder(term: "k", folder: "Sicily"), 2) == "folder “Sicily” · 2 photos")
        #expect(s(.keyword(term: "k", keyword: "Sicily"), 0) == "keyword “Sicily” · 0 photos")
        #expect(s(.manyHits, 9) == "many matches · 9 photos")
    }
}
