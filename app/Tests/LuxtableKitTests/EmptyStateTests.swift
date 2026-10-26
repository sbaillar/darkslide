import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Polish pass: the empty states say what happened and offer the next step.
@MainActor @Suite struct PolishEmptyStateTests {
    @Test func firstRunOffersACardAndAFolder() async {
        let m = makeModel(counts: [])
        await m.start()
        #expect(m.currentSession == nil && m.emptyState == .firstRun)
        #expect(m.emptyState.actions == [.importCard, .openFolder])
        #expect(m.emptyState.message.contains("memory card") && m.emptyState.message.contains("folder"))
        #expect(m.performEmptyStateAction(.importCard) && m.sheet == .importCard(nil))
        #expect(!m.performEmptyStateAction(.openFolder), "the view runs the folder panel")
    }

    @Test func statesFollowTheSessionFilterAndFocus() async throws {
        let m = makeModel(counts: [12, 0])
        await m.start()
        #expect(m.emptyState == .selectPhoto && m.emptyState.actions.isEmpty)

        // A filter that hides everything offers to clear it.
        m.filter.text = "no-such-photo-anywhere"
        #expect(m.visiblePhotos.isEmpty && m.emptyState == .noMatches)
        #expect(m.emptyState.actions.map(\.title) == ["Clear Filters"])
        m.performEmptyStateAction(.clearFilters)
        #expect(!m.filter.isActive && !m.visiblePhotos.isEmpty)

        // An empty smart folder says what would appear there.
        let f = try #require(await m.smartFolders.create(name: "Keepers", criteria: SmartCriteria(minRating: 5, pick: .picked),
                                                          photos: m.photos))
        m.selectSmartFolder(f.id)
        #expect(m.visiblePhotos.isEmpty)
        let state = m.emptyState
        #expect(state == .emptySmartFolder(name: "Keepers", criteria: f.criteria))
        #expect(state.title == "Nothing in “Keepers” yet")
        #expect(state.message == "Photos rated 5★ or more, picked appear here as soon as there are any.")
        #expect(state.actions == [.showAllPhotos])
        m.filter.text = "beach" // typed search inside the folder: it's the filters
        #expect(m.emptyState == .noMatches)
        m.performEmptyStateAction(.clearFilters) // clears the search, stays in the folder
        #expect(m.filter.text.isEmpty && m.filter.smartFolder?.id == f.id)
        #expect(m.emptyState == .emptySmartFolder(name: "Keepers", criteria: f.criteria))
        m.performEmptyStateAction(.showAllPhotos)
        #expect(m.filter.smartFolder == nil && !m.visiblePhotos.isEmpty)

        // An empty session: a card or a folder.
        let empty = try #require(m.sessionList.first { $0.id != m.currentSession?.id })
        await m.open(empty)
        #expect(m.photos.isEmpty && m.emptyState == .emptySession)
        #expect(m.emptyState.actions.map(\.title) == ["Import from Card…", "Open Folder…"])
    }

    /// (The recents state on a live launch: `MissingOriginalsTests.recentsThatCantOpenAreNeverReopened`.)
    @Test func everyStateSaysSomething() {
        #expect(EmptyState.recents.actions == [.importCard, .openFolder])
        for s in [EmptyState.loading, .recents, .firstRun, .originalsMissing, .noMatches, .emptySession, .selectPhoto,
                  .emptySmartFolder(name: "A", criteria: SmartCriteria())] {
            #expect(!s.symbol.isEmpty && !s.title.isEmpty)
        }
        #expect(EmptyState.originalsMissing.message.contains("Locate"))
    }

    @Test func smartFolderRulesInWords() {
        #expect(SmartCriteria().explanation == "Every photo in the session appears here.")
        #expect(SmartCriteria(maxRating: 0).explanation == "Photos unrated appear here as soon as there are any.")
        #expect(SmartCriteria.exactRating(3).explanation.hasPrefix("Photos rated exactly 3★"))
        #expect(SmartCriteria(minRating: 2, maxRating: 4).explanation.hasPrefix("Photos rated 2★ to 4★"))
        #expect(SmartCriteria(maxRating: 2).explanation.hasPrefix("Photos rated up to 2★"))
        let all = SmartCriteria(tags: [.blue, .red], pick: .rejected, text: " beach ", camera: "A9", lens: "85",
                                dateFrom: Date(timeIntervalSince1970: 0), isoMin: 100, isoMax: 800, edited: false,
                                keywords: ["sunset", " ", "surf"])
        let s = all.explanation
        for part in ["tagged red or blue", "rejected", "matching “beach”", "camera matching “A9”", "lens matching “85”",
                     "taken on or after", "at ISO 100–800", "not edited yet", "with the keywords “sunset”, “surf”"] {
            #expect(s.contains(part), "\(part) in \(s)")
        }
        let more = SmartCriteria(pick: PickState.none, dateFrom: Date(timeIntervalSince1970: 0),
                                 dateTo: Date(timeIntervalSince1970: 86_400 * 30), isoMin: 3200, edited: true,
                                 keywords: ["one"]).explanation
        for part in ["neither picked nor rejected", "taken ", " to ", "at ISO 3200 or more", "edited", "the keyword “one”"] {
            #expect(more.contains(part), "\(part) in \(more)")
        }
        #expect(SmartCriteria(dateTo: Date(timeIntervalSince1970: 0), isoMax: 400).explanation.contains("taken on or before"))
        #expect(SmartCriteria(isoMax: 400).explanation.contains("at ISO 400 or less"))
    }
}
