import Foundation
import ImageIO
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

private var fm: FileManager { .default }

private func tempDir(_ tag: String) throws -> URL {
    let d = fm.temporaryDirectory.appendingPathComponent("ux9-\(tag)-\(UUID().uuidString)", isDirectory: true)
    try fm.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

// MARK: - C1 / M6 / M7: onboarding archive target

@MainActor
@Suite struct Ux9OnboardingTests {
    func status(_ t: String, existing: Set<String>, writable: Bool = true) -> OnboardingModel.TargetStatus {
        OnboardingModel.status(for: t, fileExists: { existing.contains($0) }, isWritable: { _ in writable })
    }

    @Test func readyOnlyWhenTheTargetItselfExists() {
        // C1: the parent existing used to count as ready; the archive then stayed "Offline" for ever.
        #expect(status("/Volumes/pics/Luxtable", existing: ["/Volumes/pics"]) == .creatable)
        #expect(status("/Volumes/pics/Luxtable", existing: ["/Volumes/pics", "/Volumes/pics/Luxtable"]) == .ready)
        #expect(status("/Volumes/pics/Luxtable", existing: ["/Volumes/pics"], writable: false) == .missing)
        #expect(status("/Volumes/pics/Luxtable", existing: []) == .notMounted("pics"))
        #expect(status("/Users/me/a/b", existing: []) == .missing, "parent missing: can't be created")
    }

    @Test func pathsAreValidated() {
        // M6: "/" and "relative/path" were accepted; "~" wasn't expanded.
        if case .invalid = status("/", existing: ["/"]) {} else { Issue.record("/ is a whole disk") }
        if case .invalid = status("/Volumes/pics", existing: ["/Volumes/pics"]) {} else { Issue.record("a volume root") }
        if case .invalid = status("relative/path", existing: []) {} else { Issue.record("relative") }
        let home = NSHomeDirectory()
        #expect(status("~/Archive", existing: [home, home + "/Archive"]) == .ready)
        #expect(OnboardingModel.normalized("  ~/Archive ") == home + "/Archive")
        #expect(status("  ", existing: []) == .empty)
    }

    @Test func finishIsBlockedOnMissingAndInvalidTargets() async {
        let o = OnboardingModel(settings: makeSettings(), fileExists: { $0 == "/Users" }, createDirectory: { _ in },
                                isWritable: { _ in false })
        o.archiveTarget = "/Users/nobody/x"
        await o.refreshArchiveStatus()
        #expect(o.archiveStatus == .missing && !o.canFinish)
        o.archiveTarget = "relative"
        await o.refreshArchiveStatus()
        #expect(!o.canFinish)
        o.archiveEnabled = false
        #expect(o.canFinish, "archiving off: nothing to check")
        o.archiveEnabled = true
        o.archiveTarget = "/Volumes/nas/ds"
        await o.refreshArchiveStatus()
        #expect(o.archiveStatus == .notMounted("nas") && o.canFinish, "an unmounted share queues")
    }

    /// Real file system: Finish creates the chosen folder under a writable parent, keeps a
    /// bookmark for the Amazon folder (M7), and stores the expanded path.
    @Test func finishCreatesTheTargetAndBookmarksTheAmazonFolder() async throws {
        let root = try tempDir("onb")
        defer { try? fm.removeItem(at: root) }
        let amazon = root.appendingPathComponent("Amazon", isDirectory: true)
        try fm.createDirectory(at: amazon, withIntermediateDirectories: true)
        let settings = makeSettings()
        let o = OnboardingModel(settings: settings)
        o.sessionsFolder = root.appendingPathComponent("Sessions")
        o.archiveTarget = " " + root.appendingPathComponent("Archive").path + " "
        o.amazonFolder = amazon
        await o.refreshArchiveStatus()
        #expect(o.archiveStatus == .creatable && o.canFinish)
        try o.finish()
        var dir: ObjCBool = false
        #expect(fm.fileExists(atPath: root.appendingPathComponent("Archive").path, isDirectory: &dir) && dir.boolValue)
        #expect(settings.archiveTarget == root.appendingPathComponent("Archive").path)
        #expect(settings.amazonPhotosFolder == amazon && settings.amazonPhotosFolderBookmark != nil, "M7: bookmark kept")
        #expect(settings.onboardingComplete)
    }

    @Test func createItNowMakesOnlyTheLeafFolder() async throws {
        let root = try tempDir("create")
        defer { try? fm.removeItem(at: root) }
        let o = OnboardingModel(settings: makeSettings())
        o.archiveTarget = root.appendingPathComponent("Luxtable").path
        await o.refreshArchiveStatus()
        try await o.createArchiveTarget()
        #expect(o.archiveStatus == .ready)
        // Deeper paths aren't created (the parent must already be there).
        o.archiveTarget = root.appendingPathComponent("a/b").path
        await o.refreshArchiveStatus()
        #expect(o.archiveStatus == .missing)
        try await o.createArchiveTarget() // a no-op when not creatable
        #expect(!fm.fileExists(atPath: root.appendingPathComponent("a").path))
    }
}

// MARK: - C1 / L1 / L2 / L4: Archive settings and status

final class Ux9Order: @unchecked Sendable {
    private let lock = NSLock()
    private var list: [String] = []
    func note(_ e: String) { lock.withLock { list.append(e) } }
    var events: [String] { lock.withLock { list } }
}

final class Ux9Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    private var made: [String] = []
    func bump() { lock.withLock { n += 1 } }
    var count: Int { lock.withLock { n } }
    func record(_ p: String) { lock.withLock { made.append(p) } }
    var paths: [String] { lock.withLock { made } }
}

@MainActor
@Suite struct Ux9ArchiveSettingsTests {
    @Test func aMissingTargetOffersCreateAndTheSuggestionIsCreated() async {
        let s = makeSettings()
        s.archiveTarget = ""
        let made = Ux9Counter()
        let m = ArchiveSettingsModel(settings: s, checker: { p in made.paths.contains(p) ? .ready(fsType: "nfs")
                                         : .unavailable("No such file or directory (os error 2)") },
                                     exists: { $0 == "/Volumes/NAS" }, appInstalled: { true },
                                     creatable: { p in !made.paths.contains(p) && p.hasPrefix("/Volumes/NAS/") },
                                     makeFolder: { made.record($0) }, timeout: 1)
        await m.refresh()
        #expect(m.suggestion == AppSettings.defaultArchiveTarget)
        await m.useSuggestion() // choosing the suggestion is explicit: the folder is created (C1)
        #expect(made.paths == [AppSettings.defaultArchiveTarget])
        #expect(m.targetState == .ready(fsType: "nfs") && !m.canCreateTarget)

        await m.setTarget("/Volumes/NAS/Other")
        #expect(m.canCreateTarget && m.targetText.contains("doesn't exist yet"))
        await m.createTarget()
        #expect(made.paths.last == "/Volumes/NAS/Other" && m.createError == nil && m.targetState == .ready(fsType: "nfs"))
    }

    /// Both archive behaviours: today's check reports a missing folder unavailable (Create
    /// It offered); once lt-archive creates the folder under a mounted volume, the check says
    /// ready and the text says it will be created on the first sync (no Create button).
    @Test func missingFolderWordingWorksWithBothCheckResults() async {
        let s = makeSettings()
        s.archiveTarget = "/Volumes/pics/Luxtable"
        let today = ArchiveSettingsModel(settings: s, checker: { _ in .unavailable("No such file or directory (os error 2)") },
                                         exists: { $0 == "/Volumes/pics" }, appInstalled: { true },
                                         creatable: { _ in true }, timeout: 1)
        await today.checkTarget()
        #expect(today.canCreateTarget && today.targetExists == false && today.targetText.contains("doesn't exist yet"))
        let upcoming = ArchiveSettingsModel(settings: s, checker: { _ in .ready(fsType: "nfs") },
                                            exists: { $0 == "/Volumes/pics" }, appInstalled: { true },
                                            creatable: { _ in true }, timeout: 1)
        await upcoming.checkTarget()
        #expect(!upcoming.canCreateTarget)
        #expect(upcoming.targetText == "Ready: the folder is created on the first sync (nfs).")
        let there = ArchiveSettingsModel(settings: s, checker: { _ in .ready(fsType: "nfs") }, exists: { _ in true },
                                         appInstalled: { true }, creatable: { _ in false }, timeout: 1)
        await there.checkTarget()
        #expect(there.targetText == "Ready (nfs)")
    }

    @Test func createFailuresAreShown() async {
        let s = makeSettings()
        s.archiveTarget = "/Volumes/pics/X"
        let m = ArchiveSettingsModel(settings: s, checker: { _ in .unavailable("missing") }, exists: { _ in true },
                                     appInstalled: { true }, creatable: { _ in true },
                                     makeFolder: { _ in throw MockError.invalid("read-only volume") }, timeout: 1)
        await m.checkTarget()
        await m.createTarget()
        #expect(m.createError != nil)
    }

    /// L4: the Amazon app lookup no longer waits behind a slow target check: it starts
    /// before the check has finished (ordering, not wall-clock, so load can't flake it).
    @Test func amazonDetectionRunsAlongsideTheTargetCheck() async {
        let s = makeSettings()
        s.archiveTarget = "/Volumes/pics/Luxtable"
        let order = Ux9Order()
        let m = ArchiveSettingsModel(settings: s, checker: { _ in
                                         // Finishes only once the lookup has started (no wall-clock race).
                                         for _ in 0..<2000 where !order.events.contains("app lookup started") {
                                             try? await Task.sleep(for: .milliseconds(5))
                                         }
                                         order.note("check finished")
                                         return .ready(fsType: "apfs")
                                     }, exists: { _ in true },
                                     appInstalled: { order.note("app lookup started"); return true },
                                     creatable: { _ in false }, timeout: 5)
        await m.refresh()
        #expect(m.amazonAppInstalled == true)
        #expect(order.events.first == "app lookup started", "\(order.events)")
        #expect(!ArchiveSettingsModel.amazonVideosNote.isEmpty)
    }

    @Test func amazonLineIsCumulative() {
        // L2: after the first pass the line used to read "0 handed over, 40 already there".
        let a = ArchiveDetail.Amazon(folder: "/A", lastRun: Date(), placed: 0, alreadyMirrored: 40)
        #expect(ArchiveDisplay.amazonLine(a) == "Amazon Photos: 40 originals handed to the app")
        let one = ArchiveDetail.Amazon(folder: "/A", lastRun: Date(), placed: 1, alreadyMirrored: 0)
        #expect(ArchiveDisplay.amazonLine(one) == "Amazon Photos: 1 original handed to the app (1 new)")
    }
}

// MARK: - H5: File menu

@MainActor
@Suite struct Ux9MenuTests {
    @Test func liveImportWithVerifiedCopiesIsNotComingSoon() {
        let verifying = ReportingImportService(scan: ImportScan(candidates: []), make: { _ in ImportReport(filesTotal: 0, filesCopied: 0, filesVerified: 0, bytesCopied: 0) })
        let live = AppModel(sessions: MockSessionStore(counts: [3]), images: MockImageProvider(latency: .zero),
                            renderer: FakeRenderer(), importer: verifying,
                            archive: UnavailableArchiveService(target: ""), ai: MockAIService(),
                            settings: makeSettings(), mode: .live)
        #expect(CommandCatalog.title(.importCard, model: live) == "Import from Card…")
    }
}

// MARK: - M10: light-mode contrast

@Suite struct Ux9PaletteTests {
    @Test func accentStarsAndYellowMeetContrastInBothAppearances() {
        for (name, accents, greys) in [("light", AccentPalette.light, GreyPalette.light), ("dark", AccentPalette.dark, GreyPalette.dark)] {
            for pair in accents.pairs(on: greys) {
                let r = pair.color.contrast(onGrey: pair.background)
                #expect(r >= pair.minimum, "\(name) \(pair.name): \(r)")
            }
        }
        // The dark-mode amber on light greys is what failed (about 1.5:1).
        #expect(AccentPalette.dark.accent.contrast(onGrey: GreyPalette.light.chrome) < 2)
    }
}

// MARK: - M11: keyboard / VoiceOver adjustments

@Suite struct Ux9ControlAdjustTests {
    @Test func wheelHueWrapsAndStartsVisible() {
        var w = WheelValue()
        w = ControlAdjust.wheelHue(w, steps: -1)
        #expect(w.hue == 355 && w.saturation == 0.1)
        w = ControlAdjust.wheelHue(w, steps: 2)
        #expect(w.hue == 5)
        #expect(ControlAdjust.wheelSaturation(w, steps: 30).saturation == 1)
        #expect(ControlAdjust.wheelSaturation(w, steps: -30).saturation == 0)
        #expect(ControlDescriptions.wheelHue("Shadows", WheelValue()).value == "neutral")
        #expect(ControlDescriptions.wheelHue("Shadows", w).label == "Shadows hue and saturation")
    }

    @Test func curveMidtonesAddOrMoveTheMidpoint() {
        let up = ControlAdjust.curveMidtones([], steps: 2)
        #expect(up == [CurvePoint(0.5, 0.54)])
        let pts = [CurvePoint(0.25, 0.2), CurvePoint(0.52, 0.5), CurvePoint(0.8, 0.85)]
        let moved = ControlAdjust.curveMidtones(pts, steps: -1)
        #expect(moved.count == 3 && abs(moved[1].y - 0.48) < 1e-12)
        #expect(ControlAdjust.curveMidtones([CurvePoint(0.5, 0.99)], steps: 5)[0].y == 1)
    }

    @Test func levelsStepsStayOrderedAndClamped() {
        var l = LevelsEdit()
        l = ControlAdjust.levelsGamma(l, steps: 2)
        #expect(abs(l.gamma - 1.1) < 1e-12)
        #expect(ControlAdjust.levelsGamma(l, steps: -1000).gamma == 0.1)
        l = ControlAdjust.levelsInput(l, black: true, steps: 10)
        #expect(abs(l.inBlack - 10.0 / 255) < 1e-12)
        l.inWhite = l.inBlack + 2.0 / 255
        l = ControlAdjust.levelsInput(l, black: false, steps: -10)
        #expect(l.inWhite > l.inBlack, "never crosses the black point")
        l = ControlAdjust.levelsInput(l, black: true, steps: 1000)
        #expect(l.inBlack < l.inWhite)
    }
}

// MARK: - Keywords / artist / copyright

@MainActor
@Suite struct Ux9KeywordTests {
    @Test func parsingMergingAndRemoving() {
        #expect(Keywords.parse(" beach, sunset;Crete\n , Beach ") == ["beach", "sunset", "Crete"])
        #expect(Keywords.merge(["Beach"], adding: ["beach", "surf"]) == ["Beach", "surf"])
        #expect(Keywords.removing("BEACH", from: ["Beach", "surf"]) == ["surf"])
        var a = samplePhoto(1)
        a.keywords = ["Crete", "beach"]
        var b = samplePhoto(2)
        b.keywords = ["beach", "Surf"]
        #expect(Keywords.common([a, b]) == ["beach"] && Keywords.common([]) == [])
    }

    @Test func modelEditsKeywordsOnTheSelectionWithUndo() async {
        let m = makeModel(counts: [4])
        await m.start()
        m.selection = Set(m.visibleIDs.prefix(2))
        m.addKeywords("beach, sunset")
        #expect(m.selectionKeywords == ["beach", "sunset"])
        m.removeKeyword("Beach")
        #expect(m.selectionKeywords == ["sunset"])
        m.undo()
        #expect(m.selectionKeywords == ["beach", "sunset"])
        m.addKeywords("  ")
        #expect(m.selectionKeywords == ["beach", "sunset"], "nothing to add")
    }

    @Test func writePlanWritesChangedKeywordsOnly() {
        var a = samplePhoto(1)
        let stored = a
        a.keywords = ["x"]
        let plan = RustSessionStore.writePlan([a], stored: [a.id: stored])
        #expect(plan.keywords == [["x"]: [a.id.raw]] && plan.ratings.isEmpty)
        #expect(RustSessionStore.writePlan([a], stored: [a.id: a]).keywords.isEmpty)
    }

    /// Live: keywords persist in the sidecars through lt-session (real FFI), and Settings'
    /// artist / copyright are written into an exported JPEG.
    @Test func keywordsPersistAndExportsCarryArtistAndCopyright() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? fm.removeItem(at: dir) }
        let base = try tempDir("kw-store")
        defer { try? fm.removeItem(at: base) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info = try await store.add(folder: dir)
        var photo = try #require(try await store.photos(in: info).first)
        photo.keywords = ["Crete", "waterfall"]
        try await store.save(photos: [photo])
        let reopened = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info2 = try await reopened.add(folder: dir)
        #expect(try await reopened.photos(in: info2).first?.keywords == ["Crete", "waterfall"])

        let settings = makeSettings()
        settings.artist = "Alexa R"
        settings.copyright = "© 2026 Alexa R"
        let credit = ExportRecipeStore(defaults: settings.userDefaults).credit
        #expect(credit.artist == "Alexa R" && credit.copyright == "© 2026 Alexa R")
        let out = try tempDir("kw-out")
        defer { try? fm.removeItem(at: out) }
        let url = try #require(photo.url)
        let job = ExportJob(items: [ExportJobItem(photoID: photo.id, url: url, seq: 1)],
                            recipes: [ExportRecipe(name: "Small", format: .jpeg, quality: 80, sizeMode: .longEdge, sizeValue: 400)],
                            defaultFolder: out, artist: credit.artist, copyright: credit.copyright)
        let o = try await RustExportService().export(job) { (_: ExportProgressUpdate) in }
        let file = try #require(o.written.first?.url)
        let src = try #require(CGImageSourceCreateWithURL(file as CFURL, nil))
        let props = try #require(CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any])
        let tiff = props[kCGImagePropertyTIFFDictionary] as? [CFString: Any]
        #expect(tiff?[kCGImagePropertyTIFFArtist] as? String == "Alexa R")
        #expect(tiff?[kCGImagePropertyTIFFCopyright] as? String == "© 2026 Alexa R")
    }

    @Test func emptyCreditIsNotWritten() {
        let settings = makeSettings()
        settings.artist = "  "
        let credit = ExportRecipeStore(defaults: settings.userDefaults).credit
        #expect(credit.artist == nil && credit.copyright == nil)
    }
}

// MARK: - Lens profile picker

@MainActor
@Suite struct Ux9LensTests {
    @Test func profilesAreListedAndGrouped() {
        let groups = LensProfiles.grouped()
        #expect(!groups.isEmpty)
        #expect(groups.map(\.maker) == groups.map(\.maker).sorted())
        #expect(LensProfiles.all.contains { $0.model.contains("24-70") })
        let manual = LensProfiles.grouped([.init(maker: "Sony", model: "B"), .init(maker: "Sony", model: "A"),
                                           .init(maker: "Sigma", model: "C")])
        #expect(manual.map(\.maker) == ["Sigma", "Sony"] && manual[1].models == ["A", "B"])
    }

    @Test func readoutAndNoMatchGuidance() {
        let none = PhotoDevelopInfo(asShot: WhiteBalanceValue(kelvin: 5000, tint: 0), lens: "24-70mm F2.8 DG DN | Art 019",
                                    lensProfile: nil, iso: 100, renderer: "GPU")
        #expect(LensReadout.profile(info: none, exifLens: "24-70mm F2.8 DG DN | Art 019") == "No profile for 24-70mm F2.8 DG DN | Art 019")
        #expect(LensReadout.noMatchHelp(info: none, chosen: nil)?.contains("no correction profile") == true)
        #expect(LensReadout.noMatchHelp(info: none, chosen: "FE 24-70mm F2.8 GM") == nil)
        #expect(LensReadout.profile(info: none, exifLens: nil, chosen: "FE 24-70mm F2.8 GM") == "FE 24-70mm F2.8 GM (chosen)")
        var matched = none
        matched.lensProfile = "Sony FE 24-70mm F2.8 GM"
        #expect(LensReadout.noMatchHelp(info: matched, chosen: nil) == nil)
    }

    @Test func chosenProfileRoundTripsThroughTheSidecarCodec() throws {
        var e = EditState()
        e.lens.profile = "FE 24-70mm F2.8 GM"
        let (back, _) = try AdjustmentsCodec.decode(AdjustmentsCodec.encode(e))
        #expect(back.lens.profile == "FE 24-70mm F2.8 GM")
        e.lens.profile = nil
        let (auto, _) = try AdjustmentsCodec.decode(AdjustmentsCodec.encode(e))
        #expect(auto.lens.profile == nil)
    }
}

// MARK: - L10 / L9

@Suite struct Ux9ExportFolderTests {
    @Test func aSlowSessionFolderIsExplainedNotSilent() async throws {
        let home = try tempDir("home")
        defer { try? fm.removeItem(at: home) }
        let s = SessionInfo(name: "Trip", url: home.appendingPathComponent("Trip"), photoCount: 1, createdAt: Date())
        let slow = await ExportDestination.resolveExplained(for: s, timeout: 0.05, home: home) { _ in
            // Far longer than the timeout even when a loaded suite delays the timer hop.
            Thread.sleep(forTimeInterval: 5)
            return true
        }
        #expect(slow.folder.path.hasSuffix("Pictures/Luxtable Exports/Trip") && slow.note?.contains("didn't answer") == true)
        let card = await ExportDestination.resolveExplained(for: s, timeout: 5, home: home) { _ in false }
        #expect(card.note?.contains("card") == true)
        let fine = await ExportDestination.resolveExplained(for: s, timeout: 5, home: home) { _ in true }
        #expect(fine.folder.lastPathComponent == "Output" && fine.note == nil)
    }
}

@MainActor
@Suite struct Ux9SnapshotTests {
    @Test func windowSizeAndArguments() {
        #expect(WindowSnapshot.windowSize("1280x800") == CGSize(width: 1280, height: 800))
        #expect(WindowSnapshot.windowSize("1280X800") == CGSize(width: 1280, height: 800))
        #expect(WindowSnapshot.windowSize("200x100") == nil && WindowSnapshot.windowSize("wide") == nil)
        #expect(WindowSnapshot.argument("--appearance", in: ["D", "--appearance", "light"]) == "light")
        #expect(WindowSnapshot.argument("--appearance", in: ["D", "--appearance"]) == nil)
    }
}
