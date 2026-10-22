import Foundation
import Testing
@testable import DarkslideKit

@MainActor
@Suite struct OnboardingModelTests {
    @Test func stepsNavigate() {
        let o = OnboardingModel(settings: makeSettings(), fileExists: { _ in true }, createDirectory: { _ in })
        #expect(o.isFirst && !o.isLast && o.progress == 0)
        o.back()
        #expect(o.step == .welcome)
        for _ in 0..<10 { o.next() }
        #expect(o.isLast && o.step == .amazon && o.progress == 1)
        o.back()
        #expect(o.step == .archive)
        #expect(OnboardingModel.Step.allCases.allSatisfy { !$0.title.isEmpty })
    }

    @Test func archiveStatusReflectsMounts() {
        var existing: Set<String> = []
        func status(_ t: String) -> OnboardingModel.TargetStatus {
            OnboardingModel.status(for: t, fileExists: { existing.contains($0) })
        }
        #expect(status("/Volumes/pics/Darkslide") == .notMounted("pics"))
        existing = ["/Volumes/pics"]
        // C1: only the folder itself counts as ready; a missing one under a mounted share can be created.
        #expect(status("/Volumes/pics/Darkslide") == .creatable)
        existing = ["/Volumes/pics", "/Volumes/pics/Darkslide"]
        #expect(status("/Volumes/pics/Darkslide") == .ready)
        #expect(status("/Users/me/Archive") == .missing)
        existing = ["/Users/me"]
        #expect(status("/Users/me/Archive") == .creatable)
        #expect(status("   ") == .empty)
    }

    @Test func refreshRunsOffMainAndTimesOut() async {
        let paths = PathSet(["/Volumes/pics"])
        let o = OnboardingModel(settings: makeSettings(), fileExists: { paths.contains($0) }, createDirectory: { _ in })
        #expect(o.archiveStatus == .checking)
        o.archiveTarget = "/Volumes/pics/Darkslide"
        await o.refreshArchiveStatus()
        #expect(o.archiveStatus == .creatable)
        o.archiveTarget = ""
        await o.refreshArchiveStatus()
        #expect(o.archiveStatus == .empty)

        // A stat that never returns until the test releases it: the refresh must come back
        // on its own timeout (no wall-clock bound, which flaked on a loaded machine).
        let release = DispatchSemaphore(value: 0)
        let hung = OnboardingModel(settings: makeSettings(), fileExists: { _ in release.wait(); release.signal(); return true },
                                   createDirectory: { _ in }, timeout: 0.1)
        hung.archiveTarget = "/Volumes/pics/Darkslide"
        await hung.refreshArchiveStatus()
        #expect(hung.archiveStatus == .unreachable, "a hung NFS stat never blocks the refresh")
        release.signal()
    }

    @Test func suggestsTheShareOnlyWhenMounted() async {
        let settings = makeSettings()
        settings.archiveTarget = ""
        let absent = OnboardingModel(settings: settings, fileExists: { _ in false }, createDirectory: { _ in })
        await absent.suggestArchiveTarget()
        #expect(absent.archiveTarget.isEmpty)
        #expect(absent.archiveStatus == .empty)

        let mounted = OnboardingModel(settings: settings, fileExists: { $0 == "/Volumes/NAS" }, createDirectory: { _ in },
                                      isWritable: { _ in true }) // never this Mac's real volumes
        await mounted.suggestArchiveTarget()
        #expect(mounted.archiveTarget == AppSettings.defaultArchiveTarget)
        #expect(mounted.archiveStatus == .creatable, "suggested, not there yet: Finish creates it")

        let chosen = OnboardingModel(settings: settings, fileExists: { _ in true }, createDirectory: { _ in })
        chosen.archiveTarget = "/Users/me/Archive"
        await chosen.suggestArchiveTarget()
        #expect(chosen.archiveTarget == "/Users/me/Archive", "never overrides the user's choice")
    }

    @Test func finishPersistsAndCreatesFolder() async throws {
        let settings = makeSettings()
        var created: [URL] = []
        let o = OnboardingModel(settings: settings, fileExists: { _ in true }, createDirectory: { created.append($0) })
        o.sessionsFolder = URL(fileURLWithPath: "/tmp/Sessions")
        o.archiveTarget = "  /Volumes/nas/ds  "
        o.amazonFolder = URL(fileURLWithPath: "/tmp/Amazon")
        #expect(!o.canFinish, "not checked yet")
        await o.refreshArchiveStatus()
        #expect(o.canFinish)
        try o.finish()
        #expect(created == [URL(fileURLWithPath: "/tmp/Sessions")])
        #expect(settings.onboardingComplete && settings.archiveTarget == "/Volumes/nas/ds")
        #expect(settings.amazonPhotosFolder?.path == "/tmp/Amazon" && settings.sessionsFolder.path == "/tmp/Sessions")
    }

    @Test func archiveCanBeDisabledAndErrorsPropagate() {
        let settings = makeSettings()
        let o = OnboardingModel(settings: settings, fileExists: { _ in true },
                                createDirectory: { _ in throw MockError.invalid("read-only") })
        o.archiveTarget = " "
        #expect(!o.canFinish)
        o.archiveEnabled = false
        #expect(o.canFinish)
        #expect(throws: MockError.invalid("read-only")) { try o.finish() }
        #expect(!settings.onboardingComplete, "nothing saved on failure")
    }
}

final class PathSet: @unchecked Sendable {
    private let lock = NSLock()
    private var paths: Set<String>
    init(_ paths: Set<String>) { self.paths = paths }
    func contains(_ p: String) -> Bool { lock.withLock { paths.contains(p) } }
}
