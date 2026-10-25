import DarkslideCore
import Foundation
import Testing
@testable import DarkslideMac

/// Versions, the releases feed, release notes, and the updater's checks, schedule,
/// download, verification and install hand-off (`Updater`).
@MainActor
@Suite(.serialized) struct UpdaterTests {
    // MARK: - Fixtures

    /// A releases list shaped like GitHub's (`GET /repos/…/releases`): a draft, two betas, the
    /// current release, an older one without its digest, and the RAW helper's release.
    static func feedJSON(sha: String = String(repeating: "a", count: 64), size: Int64 = 1000,
                         extra: [[String: Any]] = []) throws -> Data {
        func release(_ tag: String, prerelease: Bool = false, draft: Bool = false, body: String = "## What's new\n\n- **Thing.** Did it.\n\n## Install\n\nUnzip.",
                     asset: String? = nil, digest: String?? = nil) -> [String: Any] {
            let v = tag.dropFirst()
            let name = asset ?? "Darkslide-\(v)-macos-arm64.zip"
            var a: [String: Any] = ["name": name, "size": size,
                                    "browser_download_url": "https://github.com/sbaillar/darkslide/releases/download/\(tag)/\(name)"]
            switch digest {
            case .none: a["digest"] = "sha256:\(sha)"
            case .some(.some(let d)): a["digest"] = d
            case .some(.none): break
            }
            return ["tag_name": tag, "prerelease": prerelease, "draft": draft, "body": body,
                    "html_url": "https://github.com/sbaillar/darkslide/releases/tag/\(tag)", "assets": [a]]
        }
        let list: [[String: Any]] = extra + [
            release("v0.3.0", draft: true),
            release("v0.1.19-beta.2", prerelease: true, body: "## What's new\n\n- Beta two.\n"),
            release("v0.1.19-beta.1", prerelease: true),
            release("v0.1.18"),
            release("v0.1.17", digest: .some(nil)),
            ["tag_name": "rawler-helper-1", "prerelease": false, "draft": false, "body": "helper",
             "assets": [["name": "darkslide-rawler-macos-arm64", "size": 5_787_312, "digest": "sha256:\(sha)",
                         "browser_download_url": "https://github.com/sbaillar/darkslide/releases/download/rawler-helper-1/darkslide-rawler-macos-arm64"]]],
        ]
        return try JSONSerialization.data(withJSONObject: list)
    }

    struct Feed: ReleaseFetching {
        let data: Data?
        let calls = Counter()
        func releases() async throws -> Data {
            calls.add()
            guard let data else { throw UpdateError("offline") }
            return data
        }
    }

    final class Counter: @unchecked Sendable {
        private let lock = NSLock()
        private var n = 0
        var count: Int { lock.withLock { n } }
        func add() { lock.withLock { n += 1 } }
    }

    /// What the updater asked of the system.
    final class System: @unchecked Sendable {
        private let lock = NSLock()
        private var _spawned: [(String, [String])] = []
        private var _revealed: [URL] = []
        private var _terminated = 0
        var spawned: [(String, [String])] { lock.withLock { _spawned } }
        var revealed: [URL] { lock.withLock { _revealed } }
        var terminated: Int { lock.withLock { _terminated } }

        func tools(signature: String? = nil) -> UpdateTools {
            UpdateTools(unzip: UpdateTools.live.unzip,
                        verifySignature: { _ in if let signature { throw UpdateError(signature) } },
                        spawn: { s, a in self.lock.withLock { self._spawned.append((s, a)) } },
                        reveal: { u in self.lock.withLock { self._revealed.append(u) } },
                        terminate: { self.lock.withLock { self._terminated += 1 } })
        }
    }

    struct Serves: ModelDownloader {
        let data: Data
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            try data.write(to: destination)
            progress(Int64(data.count))
        }
    }

    struct Stalls: ModelDownloader {
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            progress(1)
            while true { try await Task.sleep(for: .milliseconds(5)) }
        }
    }

    final class Clock: @unchecked Sendable {
        private let lock = NSLock()
        private var t = Date(timeIntervalSince1970: 1_800_000_000)
        var now: Date { lock.withLock { t } }
        func advance(hours: Double) { lock.withLock { t += hours * 3600 } }
    }

    static func temp(_ name: String) -> URL {
        let u = FileManager.default.temporaryDirectory.appendingPathComponent("ds-upd-\(name)-\(UUID())", isDirectory: true)
        try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
        return u
    }

    static func defaults() -> UserDefaults {
        let name = "ds-updater-\(UUID())"
        let d = UserDefaults(suiteName: name)!
        d.removePersistentDomain(forName: name)
        return d
    }

    /// A fake app bundle: Info.plist with `id` and `version`, a script for an executable.
    @discardableResult
    static func fakeApp(at url: URL, id: String = "casa.test.darkslide", version: String, marker: String = "") throws -> URL {
        let fm = FileManager.default
        try fm.createDirectory(at: url.appendingPathComponent("Contents/MacOS"), withIntermediateDirectories: true)
        let plist: [String: Any] = ["CFBundleIdentifier": id, "CFBundleShortVersionString": version,
                                    "CFBundleExecutable": "Darkslide", "CFBundlePackageType": "APPL"]
        try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
            .write(to: url.appendingPathComponent("Contents/Info.plist"))
        let exe = url.appendingPathComponent("Contents/MacOS/Darkslide")
        try Data("#!/bin/sh\n# \(marker)\nexit 0\n".utf8).write(to: exe)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: exe.path)
        return url
    }

    /// A zip of a fake app, made the way releases are (`ditto -c -k --keepParent`).
    static func zipOfApp(id: String = "casa.test.darkslide", version: String) async throws -> Data {
        let dir = temp("zip")
        defer { try? FileManager.default.removeItem(at: dir) }
        let app = try fakeApp(at: dir.appendingPathComponent("Darkslide.app"), id: id, version: version)
        let zip = dir.appendingPathComponent("a.zip")
        let (status, err) = try await UpdateTools.run("/usr/bin/ditto", ["-c", "-k", "--keepParent", app.path, zip.path])
        #expect(status == 0, "\(err)")
        return try Data(contentsOf: zip)
    }

    static func sha(_ data: Data) throws -> String {
        let f = FileManager.default.temporaryDirectory.appendingPathComponent("ds-upd-sha-\(UUID())")
        try data.write(to: f)
        defer { try? FileManager.default.removeItem(at: f) }
        return try ModelFiles.sha256(of: f)
    }

    struct Rig {
        let updater: Updater
        let support: URL
        let installed: URL
        let system: System
        let feed: Feed
        let clock: Clock
    }

    /// An updater for an app installed at `<tmp>/Applications/Darkslide.app`, version `current`.
    static func rig(current: String = "0.1.18", feed data: Data? = nil, downloader: (any ModelDownloader)? = nil,
                    signature: String? = nil) throws -> Rig {
        let base = temp("rig")
        let support = base.appendingPathComponent("Support", isDirectory: true)
        let installed = try fakeApp(at: base.appendingPathComponent("Applications/Darkslide.app"), version: current, marker: "old")
        let system = System()
        let feed = Feed(data: try data ?? feedJSON())
        let clock = Clock()
        let u = Updater(host: .init(bundle: installed, identifier: "casa.test.darkslide", version: AppVersion(current)!),
                        defaults: defaults(), support: support, feed: feed,
                        downloader: downloader ?? Serves(data: Data()), tools: system.tools(signature: signature),
                        now: { clock.now })
        return Rig(updater: u, support: support, installed: installed, system: system, feed: feed, clock: clock)
    }

    /// A rig whose feed offers `version` as a real zip of a fake app with matching digest.
    static func downloadRig(version: String = "0.1.19", id: String = "casa.test.darkslide", zipVersion: String? = nil,
                            digest: String? = nil, size: Int64? = nil, signature: String? = nil,
                            downloader: (any ModelDownloader)? = nil) async throws -> Rig {
        let zip = try await zipOfApp(id: id, version: zipVersion ?? version)
        let entry: [String: Any] = ["tag_name": "v\(version)", "prerelease": false, "draft": false,
                                    "body": "## What's new\n\n- New things.\n",
                                    "assets": [["name": "Darkslide-\(version)-macos-arm64.zip", "size": size ?? Int64(zip.count),
                                                "digest": "sha256:\(try digest ?? sha(zip))",
                                                "browser_download_url": "https://github.com/sbaillar/darkslide/releases/download/v\(version)/Darkslide.zip"]]]
        let feed = try JSONSerialization.data(withJSONObject: [entry])
        let r = try rig(feed: feed, downloader: downloader ?? Serves(data: zip), signature: signature)
        await r.updater.check(manual: true)
        return r
    }

    // MARK: - Versions

    @Test func versionsParse() {
        #expect(AppVersion("v0.1.19-beta.1")?.description == "0.1.19-beta.1")
        #expect(AppVersion("0.1.18")?.prerelease == [])
        #expect(AppVersion("0.1.19-beta.1")?.isPrerelease == true)
        #expect(AppVersion("10.20.30")?.major == 10)
        for bad in ["", "1.2", "1.2.3.4", "v1.2.x", "1.2.3-", "1.2.3-beta..1", "rawler-helper-1", "1.2.3-bé"] {
            #expect(AppVersion(bad) == nil, "\(bad)")
        }
    }

    @Test func versionsCompareBySemver() {
        let ordered = ["0.1.9", "0.1.18", "0.1.19-alpha", "0.1.19-beta", "0.1.19-beta.1", "0.1.19-beta.2",
                       "0.1.19-beta.10", "0.1.19-beta.x", "0.1.19-rc.1", "0.1.19", "0.1.20", "0.2.0", "1.0.0"]
        let vs = ordered.map { AppVersion($0)! }
        for i in vs.indices.dropLast() {
            #expect(vs[i] < vs[i + 1], "\(vs[i]) < \(vs[i + 1])")
            #expect(!(vs[i + 1] < vs[i]))
        }
        #expect(vs.shuffled().sorted() == vs)
        #expect(AppVersion("v0.1.18") == AppVersion("0.1.18"))
        #expect(!(AppVersion("0.1.18")! < AppVersion("0.1.18")!))
    }

    // MARK: - The feed

    @Test func onlyAppReleasesWithTheMacAssetAndDigestCount() throws {
        let all = try ReleaseFeed.releases(from: Self.feedJSON())
        // The draft, v0.1.17 (no digest) and rawler-helper-1 are left out.
        #expect(all.map(\.version.description) == ["0.1.19-beta.2", "0.1.19-beta.1", "0.1.18"])
        let beta = try #require(all.first)
        #expect(beta.isPrerelease)
        #expect(beta.asset.name == "Darkslide-0.1.19-beta.2-macos-arm64.zip")
        #expect(beta.asset.sha256 == String(repeating: "a", count: 64))
        #expect(beta.asset.size == 1000)
        #expect(beta.notes == ["Beta two."])
        #expect(beta.page?.absoluteString.hasSuffix("/tag/v0.1.19-beta.2") == true)
        // A digest that isn't SHA-256, a wrong asset name or plain http: not an update.
        let odd = try Self.feedJSON(extra: [
            ["tag_name": "v9.0.0", "prerelease": false, "draft": false, "assets": [["name": "Darkslide-9.0.0-macos-arm64.zip",
                "size": 1, "digest": "md5:abc", "browser_download_url": "https://github.com/x/a.zip"]]],
            ["tag_name": "v9.0.1", "prerelease": false, "draft": false, "assets": [["name": "Darkslide-9.0.1-linux.zip",
                "size": 1, "digest": "sha256:\(String(repeating: "b", count: 64))", "browser_download_url": "https://github.com/x/a.zip"]]],
            ["tag_name": "v9.0.2", "prerelease": false, "draft": false, "assets": [["name": "Darkslide-9.0.2-macos-arm64.zip",
                "size": 1, "digest": "sha256:\(String(repeating: "b", count: 64))", "browser_download_url": "http://github.com/x/a.zip"]]],
            // Not on github.com.
            ["tag_name": "v9.0.3", "prerelease": false, "draft": false, "assets": [["name": "Darkslide-9.0.3-macos-arm64.zip",
                "size": 1, "digest": "sha256:\(String(repeating: "b", count: 64))",
                "browser_download_url": "https://github.com.example.org/x/a.zip"]]],
        ])
        #expect(try ReleaseFeed.releases(from: odd).map(\.version.description) == ["0.1.19-beta.2", "0.1.19-beta.1", "0.1.18"])
        #expect(throws: (any Error).self) { try ReleaseFeed.releases(from: Data("{}".utf8)) }
        #expect(ReleaseFeed.sha256("sha256:" + String(repeating: "A", count: 64)) == String(repeating: "a", count: 64))
        #expect(ReleaseFeed.sha256("sha256:xyz") == nil)
        #expect(ReleaseFeed.sha256(nil) == nil)
    }

    @Test func theChannelPicksTheNewest() throws {
        let all = try ReleaseFeed.releases(from: Self.feedJSON())
        let v = { AppVersion($0)! }
        #expect(ReleaseFeed.newest(all, after: v("0.1.18"), beta: false) == nil, "betas aren't offered on the stable channel")
        #expect(ReleaseFeed.newest(all, after: v("0.1.18"), beta: true)?.version == v("0.1.19-beta.2"))
        #expect(ReleaseFeed.newest(all, after: v("0.1.17"), beta: false)?.version == v("0.1.18"))
        #expect(ReleaseFeed.newest(all, after: v("0.1.19-beta.1"), beta: true)?.version == v("0.1.19-beta.2"))
        #expect(ReleaseFeed.newest(all, after: v("0.1.19-beta.2"), beta: true) == nil)
        // A beta user back on the stable channel gets the release once it's out, not another beta.
        #expect(ReleaseFeed.newest(all, after: v("0.1.19-beta.1"), beta: false) == nil)
        let release = try ReleaseFeed.releases(from: Self.feedJSON(extra: [
            ["tag_name": "v0.1.19", "prerelease": false, "draft": false, "body": "",
             "assets": [["name": "Darkslide-0.1.19-macos-arm64.zip", "size": 1, "digest": "sha256:\(String(repeating: "c", count: 64))",
                         "browser_download_url": "https://github.com/x/a.zip"]]],
        ]))
        #expect(ReleaseFeed.newest(release, after: v("0.1.19-beta.2"), beta: false)?.version == v("0.1.19"))
        #expect(ReleaseFeed.newest(release, after: v("0.1.19-beta.2"), beta: true)?.version == v("0.1.19"))
        #expect(UpdateRelease.assetName(v("0.1.19-beta.1")) == "Darkslide-0.1.19-beta.1-macos-arm64.zip")
    }

    // MARK: - Release notes

    /// The v0.1.18 release's body, shortened.
    static let realBody = """
        ## What's new

        - **RAW helper.** rawler (LGPL-2.1) is no longer built into Darkslide. It's a separate, optional program, `darkslide-rawler`, \
        that Darkslide downloads on demand ([rawler-helper-1](https://github.com/sbaillar/darkslide/releases/tag/rawler-helper-1)).
        - **Bigger filmstrip pictures.** A size slider in the filmstrip's filter bar, plus ⌃⌘= and ⌃⌘-.
        - **AI Cull says why.** Every AI reject carries a short note
          shown in the info panel.

        ## Install

        macOS 26, Apple silicon. Unzip and move **Darkslide.app** to Applications.
        """

    @Test func notesAreTheWhatsNewBulletsInPlainText() {
        let notes = ReleaseNotes.short(Self.realBody)
        #expect(notes.count == 3)
        #expect(notes[0].hasPrefix("RAW helper. rawler (LGPL-2.1) is no longer built into Darkslide."))
        #expect(notes[0].contains("program, darkslide-rawler, that"))
        #expect(notes[0].contains("on demand (rawler-helper-1)."))
        #expect(notes[1] == "Bigger filmstrip pictures. A size slider in the filmstrip's filter bar, plus ⌃⌘= and ⌃⌘-.")
        #expect(notes[2] == "AI Cull says why. Every AI reject carries a short note shown in the info panel.", "continuation lines join")
        #expect(!notes.joined().contains("Install"))
        // Other spellings of the heading; Windows line ends.
        #expect(ReleaseNotes.short("### What’s New\r\n* One\r\n* Two\r\n") == ["One", "Two"])
        #expect(ReleaseNotes.short("# whats new\n- x") != ["x"], "only “What's new”")
    }

    @Test func notesWithoutTheSectionAreTheFirstTenLines() {
        let body = (1...14).map { "Line \($0)" }.joined(separator: "\n\n")
        #expect(ReleaseNotes.short("# Darkslide\n\n" + body) == ["Darkslide"] + (1...9).map { "Line \($0)" })
        #expect(ReleaseNotes.short("") == [])
        #expect(ReleaseNotes.short("## What's new\n\nJust text.\n") == ["Just text."])
    }

    // MARK: - Checking

    @Test func aManualCheckShowsTheUpdateOrUpToDate() async throws {
        let r = try Self.rig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let u = r.updater
        #expect(u.isAvailable)
        #expect(u.currentVersion == "0.1.18")
        #expect(u.automatic, "on by default")
        #expect(!u.includeBeta, "off by default")
        #expect(u.lastChecked == nil)
        await u.check(manual: true)
        #expect(u.state == .upToDate, "betas aren't offered unless asked for")
        #expect(u.lastChecked == r.clock.now)
        #expect(UpdateText.title(u.state) == "You’re up to date")
        #expect(UpdateText.detail(u.state, current: "0.1.18", beta: false) == "Darkslide 0.1.18 is the newest version.")
        u.includeBeta = true
        await u.check(manual: true)
        guard case .available(let rel) = u.state else { Issue.record("\(u.state)"); return }
        #expect(rel.version.description == "0.1.19-beta.2")
        #expect(u.notice == nil, "a manual check shows the sheet, not the notice")
        #expect(UpdateText.title(u.state) == "Darkslide 0.1.19-beta.2 is available")
        #expect(UpdateText.available(rel, current: "0.1.18") == "Darkslide 0.1.19-beta.2 is available (you have 0.1.18).")
        // Settings are remembered.
        let again = Updater(host: u.host, defaults: u.defaults, support: r.support, feed: r.feed)
        #expect(again.includeBeta)
        #expect(again.lastChecked == r.clock.now)
    }

    @Test func checkNowOpensTheSheet() async throws {
        let r = try Self.rig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        r.updater.checkNow()
        #expect(r.updater.sheetShown)
        #expect(await waitUntil { r.updater.state == .upToDate })
        r.updater.later()
        #expect(!r.updater.sheetShown)
        #expect(r.updater.state == .idle)
    }

    @Test func aFailedCheckIsSaidOnlyWhenAsked() async throws {
        let r = try Self.rig(feed: nil)
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let offline = Updater(host: r.updater.host, defaults: Self.defaults(), support: r.support, feed: Feed(data: nil))
        await offline.check(manual: false)
        #expect(offline.state == .idle, "an automatic check never interrupts")
        #expect(offline.lastError == "offline")
        #expect(UpdateText.status(offline) == "The last check didn’t work: offline")
        await offline.check(manual: true)
        #expect(offline.state == .failed("Darkslide couldn’t check for updates: offline"))
        #expect(UpdateText.status(offline) == "Darkslide couldn’t check for updates: offline")
        #expect(offline.lastChecked == nil)
    }

    @Test func skippedVersionsStayQuietUntilAskedFor() async throws {
        let r = try Self.rig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let u = r.updater
        u.includeBeta = true
        await u.check(manual: false)
        #expect(u.notice?.version.description == "0.1.19-beta.2", "an automatic check shows the notice")
        u.showSheet()
        #expect(u.sheetShown)
        #expect(u.notice == nil)
        u.skip()
        #expect(u.skipped == "0.1.19-beta.2")
        #expect(u.state == .idle)
        #expect(!u.sheetShown)
        await u.check(manual: false)
        #expect(u.notice == nil)
        #expect(u.state == .idle)
        await u.check(manual: true)
        guard case .available(let rel) = u.state else { Issue.record("\(u.state)"); return }
        #expect(rel.version.description == "0.1.19-beta.2", "a manual check still shows it")
        // Remembered.
        #expect(Updater(host: u.host, defaults: u.defaults, support: r.support, feed: r.feed).skipped == "0.1.19-beta.2")
        // Later keeps the offer; skip() outside an offer does nothing.
        u.later()
        guard case .available = u.state else { Issue.record("\(u.state)"); return }
        await u.check(manual: false) // the skipped version found again: back to quiet
        #expect(u.state == .idle)
        u.skip()
        #expect(u.state == .idle)
    }

    @Test func automaticChecksRunAtMostDaily() async throws {
        let r = try Self.rig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let u = r.updater
        #expect(u.isDue, "never checked")
        await u.tick()
        #expect(r.feed.calls.count == 1)
        #expect(!u.isDue)
        r.clock.advance(hours: 23)
        await u.tick()
        #expect(r.feed.calls.count == 1, "not again within a day")
        r.clock.advance(hours: 1)
        #expect(u.isDue)
        await u.tick()
        #expect(r.feed.calls.count == 2)
        r.clock.advance(hours: 48)
        u.automatic = false
        #expect(!u.isDue)
        await u.tick()
        #expect(r.feed.calls.count == 2, "off: never")
        #expect(UpdateText.status(u).hasPrefix("Last checked "))
    }

    @Test func theScheduleChecksAfterLaunchThenHourlyWhenDue() async throws {
        let r = try Self.rig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let sleeps = Counter()
        let clock = r.clock
        r.updater.startAutomaticChecks(sleep: { d in
            sleeps.add()
            if d == Updater.poll { clock.advance(hours: 12) } // each poll: half a day later
            if sleeps.count > 5 { throw CancellationError() }
        })
        r.updater.startAutomaticChecks(sleep: { _ in Issue.record("a second schedule") })
        #expect(await waitUntil { sleeps.count > 5 })
        // Launch, then polls at +12 h (not due), +24 h (due), +36 h (not), +48 h (due).
        #expect(await waitUntil { r.feed.calls.count == 3 })
        r.updater.stopAutomaticChecks()
        // Off: the launch check doesn't run either.
        let off = try Self.rig()
        defer { try? FileManager.default.removeItem(at: off.support.deletingLastPathComponent()) }
        off.updater.automatic = false
        let done = Counter()
        off.updater.startAutomaticChecks(sleep: { _ in done.add(); if done.count > 1 { throw CancellationError() } })
        #expect(await waitUntil { done.count > 1 })
        #expect(off.feed.calls.count == 0)
        // Without an installed app there's nothing to check.
        let none = Updater(host: nil, defaults: Self.defaults(), feed: Feed(data: nil))
        none.startAutomaticChecks(sleep: { _ in Issue.record("scheduled without an app") })
        none.checkNow()
        #expect(!none.sheetShown)
        #expect(!none.isDue)
        #expect(none.launched() == nil)
        #expect(UpdateText.status(none) == "Updates are available only in the installed app.")
    }

    // MARK: - Download and verify

    @Test func aVerifiedDownloadWaitsReadyAndRestartQuits() async throws {
        let r = try await Self.downloadRig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let u = r.updater
        await u.download(restart: false)
        guard case .ready(let rel) = u.state else { Issue.record("\(u.state)"); return }
        #expect(rel.version.description == "0.1.19")
        let ready = try #require(u.readyApp)
        #expect(FileManager.default.fileExists(atPath: ready.appendingPathComponent("Contents/Info.plist").path))
        // Only the verified app is left: no zip, no staging folder.
        #expect(try FileManager.default.contentsOfDirectory(atPath: r.support.appendingPathComponent("Updates").path) == ["Ready"])
        #expect(r.system.terminated == 0, "Download only: nothing quits")
        #expect(UpdateText.status(u) == "Darkslide 0.1.19 installs when you quit.")
        // A check meanwhile keeps the ready update.
        await u.check(manual: false)
        #expect(u.state == .ready(rel))
        u.restart()
        #expect(r.system.terminated == 1)
        #expect(u.relaunchRequested)
        #expect(FileManager.default.fileExists(atPath: r.installed.path), "the running app is never touched")
    }

    @Test func downloadAndRestartQuitsOnceVerified() async throws {
        let r = try await Self.downloadRig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        await r.updater.download(restart: true)
        #expect(r.system.terminated == 1)
        #expect(r.updater.relaunchRequested)
    }

    /// Every failure: a clear message, nothing installed, no staging left.
    @Test(arguments: [
        ("checksum", "The download isn’t the file GitHub published (checksum mismatch). Nothing was installed."),
        ("identifier", "The download isn’t Darkslide (its identifier is “com.example.other”). Nothing was installed."),
        ("version", "The download is version “0.1.20”, not 0.1.19. Nothing was installed."),
        ("signature", "The downloaded app’s signature doesn’t check out: invalid signature"),
        ("size", "The download isn’t the expected size. Nothing was installed."),
        ("download", "The update couldn’t be downloaded: HTTP 404 for Darkslide.zip"),
    ])
    func aBadDownloadInstallsNothing(_ c: (String, String)) async throws {
        struct Fails: ModelDownloader {
            func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
                throw SegmentationError.download("HTTP 404 for Darkslide.zip")
            }
        }
        let zip = try await Self.zipOfApp(version: "0.1.19")
        let r: Rig = switch c.0 {
        case "checksum": try await Self.downloadRig(digest: String(repeating: "0", count: 64))
        case "identifier": try await Self.downloadRig(id: "com.example.other")
        case "version": try await Self.downloadRig(zipVersion: "0.1.20")
        case "signature": try await Self.downloadRig(signature: "The downloaded app’s signature doesn’t check out: invalid signature")
        case "size": try await Self.downloadRig(size: Int64(zip.count) + 10)
        default: try await Self.downloadRig(downloader: Fails())
        }
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        await r.updater.download(restart: true)
        #expect(r.updater.state == .failed(c.1))
        #expect(UpdateText.title(r.updater.state) == "The update didn’t work")
        #expect(r.updater.readyApp == nil)
        let left = (try? FileManager.default.contentsOfDirectory(atPath: r.support.appendingPathComponent("Updates").path)) ?? []
        #expect(left.isEmpty, "\(left)")
        #expect(r.system.terminated == 0)
        // Quitting now installs nothing.
        r.updater.applicationWillTerminate(pid: 1)
        #expect(r.system.spawned.isEmpty)
    }

    @Test func cancellingADownloadKeepsTheOffer() async throws {
        let r = try await Self.downloadRig(downloader: Stalls())
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let u = r.updater
        let run = Task { await u.download(restart: true) }
        #expect(await waitUntil { if case .downloading = u.state { return true }; return false })
        #expect(u.isBusy)
        #expect(UpdateText.status(u).hasPrefix("Downloading Darkslide 0.1.19… "))
        #expect(UpdateText.title(u.state) == "Downloading Darkslide 0.1.19…")
        u.checkNow() // shows the download, starts nothing
        u.cancelDownload()
        await run.value
        guard case .available = u.state else { Issue.record("\(u.state)"); return }
        #expect(r.system.terminated == 0)
        #expect(((try? FileManager.default.contentsOfDirectory(atPath: r.support.appendingPathComponent("Updates").path)) ?? []).isEmpty)
    }

    // MARK: - Install and resume

    @Test func quittingWithAReadyUpdateStartsTheRelauncherAndKeepsThePlace() async throws {
        let r = try await Self.downloadRig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let u = r.updater
        var place = ResumeState()
        place.sessionPath = "/photos/2026-10-01 Hike"
        place.focusedName = "DSC01234.ARW"
        u.resumeState = { v, n in
            var p = place
            p.updatedTo = v
            p.notes = n
            return p
        }
        await u.download(restart: false)
        // Restart, then the quit is cancelled: nothing spawned, still ready.
        u.restart()
        u.quitCancelled()
        #expect(!u.relaunchRequested)
        #expect(r.system.spawned.isEmpty)
        guard case .ready = u.state else { Issue.record("\(u.state)"); return }
        // A plain quit later installs it, without reopening.
        u.applicationWillTerminate(pid: 4242)
        let (script, args) = try #require(r.system.spawned.first)
        #expect(script == UpdateTools.relauncher)
        #expect(args == ["4242", r.installed.path, u.readyApp!.path,
                         r.support.appendingPathComponent("Previous Version/Darkslide.app").path, "0", "/usr/bin/open"])
        let saved = try #require(ResumeStore(url: r.support.appendingPathComponent("resume.json")).consume())
        #expect(saved.focusedName == "DSC01234.ARW")
        #expect(saved.updatedTo == "0.1.19")
        #expect(saved.notes == ["New things."])
    }

    @Test func restartAsksToReopen() async throws {
        let r = try await Self.downloadRig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        await r.updater.download(restart: true)
        r.updater.applicationWillTerminate(pid: 7)
        #expect(r.system.spawned.first?.1[4] == "1")
    }

    @Test func aFolderDarkslideCantWriteGetsTheAppShownInFinder() async throws {
        let r = try await Self.downloadRig()
        let apps = r.installed.deletingLastPathComponent()
        defer {
            try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: apps.path)
            try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent())
        }
        await r.updater.download(restart: false)
        try FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: apps.path)
        r.updater.restart()
        guard case .manualInstall(_, let app) = r.updater.state else { Issue.record("\(r.updater.state)"); return }
        #expect(r.system.revealed == [app])
        #expect(r.system.terminated == 0)
        #expect(UpdateText.title(r.updater.state) == "Install Darkslide 0.1.19 by hand")
        #expect(UpdateText.detail(r.updater.state, current: "0.1.18", beta: false).contains("drag it to your Applications folder"))
        r.updater.applicationWillTerminate(pid: 1)
        #expect(r.system.spawned.isEmpty, "never a relauncher that can't move the app")
    }

    @Test func aRelauncherThatCantStartLeavesNoResume() async throws {
        let r = try await Self.downloadRig()
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        var tools = r.system.tools()
        tools.spawn = { _, _ in throw UpdateError("no shell") }
        let u = Updater(host: r.updater.host, defaults: Self.defaults(), support: r.support,
                        feed: r.updater.feed, downloader: r.updater.downloader, tools: tools)
        await u.check(manual: true)
        await u.download(restart: false)
        u.applicationWillTerminate(pid: 1)
        #expect(!FileManager.default.fileExists(atPath: r.support.appendingPathComponent("resume.json").path))
    }

    @Test func theNextLaunchSaysWhatsNewOnceAndClearsDownloads() throws {
        let r = try Self.rig(current: "0.1.19")
        defer { try? FileManager.default.removeItem(at: r.support.deletingLastPathComponent()) }
        let store = ResumeStore(url: r.support.appendingPathComponent("resume.json"))
        var state = ResumeState()
        state.updatedTo = "0.1.19"
        state.notes = ["One", "Two"]
        try store.save(state)
        let leftover = r.support.appendingPathComponent("Updates/.staging-x")
        try FileManager.default.createDirectory(at: leftover, withIntermediateDirectories: true)
        #expect(r.updater.launched() == state)
        #expect(r.updater.whatsNew == Updater.WhatsNew(version: "0.1.19", notes: ["One", "Two"]))
        #expect(r.updater.whatsNew?.id == "0.1.19")
        #expect(!FileManager.default.fileExists(atPath: leftover.path))
        r.updater.whatsNew = nil
        #expect(r.updater.launched() == nil, "once")
        #expect(r.updater.whatsNew == nil)
        // The install didn't happen (the old version came back): the place, but no "Updated to".
        try store.save(state)
        let old = try Self.rig(current: "0.1.18")
        defer { try? FileManager.default.removeItem(at: old.support.deletingLastPathComponent()) }
        let u = Updater(host: old.updater.host, defaults: Self.defaults(), support: r.support, feed: old.feed)
        #expect(u.launched()?.notes == ["One", "Two"])
        #expect(u.whatsNew == nil)
    }

    // MARK: - The relauncher, for real

    /// Runs the relauncher like the app does, against temp folders; returns its exit status
    /// and what the opener was asked to open.
    static func runRelauncher(old: URL, new: URL, backup: URL, relaunch: Bool, in dir: URL) async throws -> (Int32, String) {
        let log = dir.appendingPathComponent("opened.log")
        let opener = dir.appendingPathComponent("opener")
        // Logs next to itself (the folder's name has quotes and a `$` in it).
        try Data("#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$(dirname \"$0\")/opened.log\"\n".utf8).write(to: opener)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: opener.path)
        let sleeper = Process() // stands in for the quitting app
        sleeper.executableURL = URL(fileURLWithPath: "/bin/sleep")
        sleeper.arguments = ["0.3"]
        try sleeper.run()
        let (status, _) = try await UpdateTools.run("/bin/sh", ["-c", UpdateTools.relauncher, "darkslide-update",
                                                                String(sleeper.processIdentifier), old.path, new.path, backup.path,
                                                                relaunch ? "1" : "0", opener.path])
        #expect(!sleeper.isRunning, "it waited for the app to exit")
        return (status, (try? String(contentsOf: log, encoding: .utf8)) ?? "")
    }

    static func marker(_ app: URL) -> String? {
        (try? String(contentsOf: app.appendingPathComponent("Contents/MacOS/Darkslide"), encoding: .utf8))?
            .split(separator: "\n").dropFirst().first.map(String.init)
    }

    @Test func theRelauncherSwapsTheAppAndKeepsOneBackup() async throws {
        let dir = Self.temp("swap with spaces 'and' \"quotes\" $HOME")
        defer { try? FileManager.default.removeItem(at: dir) }
        let old = try Self.fakeApp(at: dir.appendingPathComponent("My Apps/Darkslide.app"), version: "0.1.18", marker: "old")
        let new = try Self.fakeApp(at: dir.appendingPathComponent("Support/Updates/Ready/Darkslide.app"), version: "0.1.19", marker: "new")
        let backup = dir.appendingPathComponent("Support/Previous Version/Darkslide.app")
        try Self.fakeApp(at: backup, version: "0.1.17", marker: "older")
        let (status, opened) = try await Self.runRelauncher(old: old, new: new, backup: backup, relaunch: true, in: dir)
        #expect(status == 0)
        #expect(Self.marker(old) == "# new", "the new app is at the old one's exact path")
        #expect(Self.marker(backup) == "# old", "the replaced app is the one backup (the older one is gone)")
        #expect(!FileManager.default.fileExists(atPath: new.path))
        #expect(opened == old.path + "\n")
        let left = try FileManager.default.contentsOfDirectory(atPath: old.deletingLastPathComponent().path)
        #expect(left == ["Darkslide.app"], "no staged or set-aside copy is left next to the app")
    }

    /// The old app is never deleted before the new one is in place: if it can't go to the
    /// backup folder, it stays next to the new one.
    @Test func theRelauncherKeepsTheOldAppWhenTheBackupCantBeWritten() async throws {
        let dir = Self.temp("backup refused")
        let locked = dir.appendingPathComponent("Locked")
        defer {
            try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: locked.path)
            try? FileManager.default.removeItem(at: dir)
        }
        let old = try Self.fakeApp(at: dir.appendingPathComponent("Applications/Darkslide.app"), version: "0.1.18", marker: "old")
        let new = try Self.fakeApp(at: dir.appendingPathComponent("Ready/Darkslide.app"), version: "0.1.19", marker: "new")
        try FileManager.default.createDirectory(at: locked, withIntermediateDirectories: true)
        try FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: locked.path)
        let backup = locked.appendingPathComponent("Previous Version/Darkslide.app")
        let (status, _) = try await Self.runRelauncher(old: old, new: new, backup: backup, relaunch: false, in: dir)
        #expect(status == 0)
        #expect(Self.marker(old) == "# new")
        let apps = old.deletingLastPathComponent()
        let aside = try FileManager.default.contentsOfDirectory(atPath: apps.path).filter { $0.hasPrefix(".Darkslide.app.previous-") }
        #expect(aside.count == 1)
        #expect(Self.marker(apps.appendingPathComponent(aside.first ?? "")) == "# old", "kept, hidden next to the new one")
    }

    @Test func theRelauncherPutsTheOldAppBackWhenTheMoveFails() async throws {
        let dir = Self.temp("rollback")
        let ready = dir.appendingPathComponent("Ready")
        defer {
            try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: ready.path)
            try? FileManager.default.removeItem(at: dir)
        }
        let old = try Self.fakeApp(at: dir.appendingPathComponent("Applications/Darkslide.app"), version: "0.1.18", marker: "old")
        let new = try Self.fakeApp(at: ready.appendingPathComponent("Darkslide.app"), version: "0.1.19", marker: "new")
        let backup = dir.appendingPathComponent("Previous Version/Darkslide.app")
        // The new app can't be moved out of its folder.
        try FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: ready.path)
        let (status, opened) = try await Self.runRelauncher(old: old, new: new, backup: backup, relaunch: true, in: dir)
        #expect(status == 6)
        #expect(Self.marker(old) == "# old", "the old app is back where it was")
        #expect(!FileManager.default.fileExists(atPath: backup.path))
        #expect(opened == old.path + "\n", "and reopened")
        // A missing new app: nothing moves; no reopen after a plain quit.
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: ready.path)
        let (gone, none) = try await Self.runRelauncher(old: old, new: dir.appendingPathComponent("nope.app"), backup: backup,
                                                        relaunch: false, in: dir)
        #expect(gone == 4)
        #expect(none == old.path + "\n", "the log is unchanged: not reopened")
        #expect(Self.marker(old) == "# old")
    }

    @Test func theRelauncherRefusesEmptyOrOddArguments() async throws {
        for args in [["1", "", "/b.app", "/c.app", "0", "/usr/bin/true"], ["1", "/a.app", "/b.app", "", "0", "/usr/bin/true"],
                     ["1", "/a.app", "/b.app", "/", "0", "/usr/bin/true"]] {
            let (status, _) = try await UpdateTools.run("/bin/sh", ["-c", UpdateTools.relauncher, "darkslide-update"] + args)
            #expect(status == 2, "\(args)")
        }
        // Every path variable is used quoted.
        let s = UpdateTools.relauncher
        for v in ["old", "new", "backup", "pid", "opener", "relaunch"] {
            let uses = s.components(separatedBy: "$\(v)").count - 1
            let quoted = s.components(separatedBy: "\"$\(v)\"").count - 1
            #expect(uses == quoted, "$\(v) unquoted")
        }
        #expect(!s.contains("rm -rf \"$old\"") && !s.contains("rm -rf \"$new\""))
    }

    // MARK: - System tools

    @Test func theLiveToolsUnpackAndCheckSignatures() async throws {
        let dir = Self.temp("tools")
        defer { try? FileManager.default.removeItem(at: dir) }
        let zip = dir.appendingPathComponent("a.zip")
        try await Self.zipOfApp(version: "1.0.0").write(to: zip)
        try await UpdateTools.live.unzip(zip, dir.appendingPathComponent("out"))
        let app = dir.appendingPathComponent("out/Darkslide.app")
        #expect(FileManager.default.fileExists(atPath: app.appendingPathComponent("Contents/Info.plist").path))
        await #expect(throws: UpdateError.self) { try await UpdateTools.live.unzip(app, dir.appendingPathComponent("bad")) }
        // Unsigned: refused. Ad-hoc signed (as releases are): accepted. Changed after signing: refused.
        await #expect(throws: UpdateError.self) { try await UpdateTools.live.verifySignature(app) }
        let (signed, _) = try await UpdateTools.run("/usr/bin/codesign", ["--force", "--sign", "-", app.path])
        #expect(signed == 0)
        try await UpdateTools.live.verifySignature(app)
        try Data("#!/bin/sh\nexit 1\n".utf8).write(to: app.appendingPathComponent("Contents/MacOS/Darkslide"))
        await #expect(throws: UpdateError.self) { try await UpdateTools.live.verifySignature(app) }
        // The relauncher's launcher: /bin/sh -c with the paths as arguments, not waited for.
        let mark = dir.appendingPathComponent("spawned it")
        try UpdateTools.live.spawn("printf '%s' \"$2\" > \"$1\"", [mark.path, "a b"])
        #expect(await waitUntil { (try? String(contentsOf: mark, encoding: .utf8)) == "a b" })
        // A zip with two apps, or none: not Darkslide.
        let two = dir.appendingPathComponent("two")
        try Self.fakeApp(at: two.appendingPathComponent("unpacked/A.app"), version: "1.0.0")
        try Self.fakeApp(at: two.appendingPathComponent("unpacked/B.app"), version: "1.0.0")
        let z = two.appendingPathComponent("z.zip")
        try Data(count: 4).write(to: z)
        var tools = UpdateTools.live
        tools.unzip = { _, _ in }
        let rel = UpdateRelease(version: AppVersion("1.0.0")!, isPrerelease: false, notes: [], page: nil,
                                asset: .init(name: "z.zip", url: URL(string: "https://x.invalid/z.zip")!, size: 4, sha256: ""))
        await #expect(throws: UpdateError("The download doesn’t contain Darkslide.")) {
            try await Updater.verify(zip: z, in: two, release: rel, identifier: "casa.test.darkslide", running: dir,
                                     tools: tools)
        }
    }

    @Test func anUpdateMustBeSignedLikeTheRunningApp() async throws {
        let teamA = CodeSigner.team("AAAA"), teamB = CodeSigner.team("BBBB")
        let certA = CodeSigner.certificate(Data([1])), certB = CodeSigner.certificate(Data([2]))
        #expect(teamA.accepts(teamA) && certA.accepts(certA))
        #expect(!teamA.accepts(teamB) && !teamA.accepts(.adHoc) && !teamA.accepts(certA))
        #expect(!certA.accepts(certB) && !certA.accepts(.adHoc) && !certA.accepts(teamA))
        #expect(CodeSigner.adHoc.accepts(.adHoc) && CodeSigner.adHoc.accepts(teamA), "an ad-hoc app has nothing to pin")
        #expect(CodeSigner.signer(team: "T", leaf: Data([1])) == .team("T"))
        #expect(CodeSigner.signer(team: "", leaf: Data([1])) == .certificate(Data([1])))
        #expect(CodeSigner.signer(team: nil, leaf: nil) == .adHoc)
        // Real signatures: Apple's own binaries have a certificate; an ad-hoc signed copy has none.
        let dir = Self.temp("signers")
        defer { try? FileManager.default.removeItem(at: dir) }
        let copy = dir.appendingPathComponent("true")
        try FileManager.default.copyItem(at: URL(fileURLWithPath: "/usr/bin/true"), to: copy)
        let (signed, _) = try await UpdateTools.run("/usr/bin/codesign", ["--force", "--sign", "-", copy.path])
        #expect(signed == 0)
        #expect(try CodeSigner.of(copy) == .adHoc)
        if case .certificate = try CodeSigner.of(URL(fileURLWithPath: "/usr/bin/true")) {} else {
            Issue.record("an Apple binary has a certificate")
        }
        #expect(throws: UpdateError.self) { try CodeSigner.of(dir.appendingPathComponent("missing")) }
        // The download is checked against the running app's signer.
        let zip = dir.appendingPathComponent("u.zip")
        try await Self.zipOfApp(version: "1.0.0").write(to: zip)
        let size = Int64(try Data(contentsOf: zip).count)
        let rel = UpdateRelease(version: AppVersion("1.0.0")!, isPrerelease: false, notes: [], page: nil,
                                asset: .init(name: "u.zip", url: URL(string: "https://github.com/x/u.zip")!, size: size, sha256: ""))
        let running = dir.appendingPathComponent("Running.app")
        var tools = System().tools()
        tools.signer = { $0 == running ? teamA : teamB }
        await #expect(throws: UpdateError("The download isn’t signed the way this copy of Darkslide is. Nothing was installed.")) {
            try await Updater.verify(zip: zip, in: dir.appendingPathComponent("s1"), release: rel,
                                     identifier: "casa.test.darkslide", running: running, tools: tools)
        }
        tools.signer = { _ in teamA }
        let app = try await Updater.verify(zip: zip, in: dir.appendingPathComponent("s2"), release: rel,
                                           identifier: "casa.test.darkslide", running: running, tools: tools)
        #expect(app.lastPathComponent == "Darkslide.app")
    }

    @Test func theFeedIsFetchedOverHTTPSWithOnlyTheVersion() async throws {
        let dir = Self.temp("feed")
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent("releases.json")
        try Self.feedJSON().write(to: file)
        let data = try await GitHubReleaseFetcher(version: "0.1.18", url: file, schemes: ["file"]).releases()
        #expect(try ReleaseFeed.releases(from: data).count == 3)
        await #expect(throws: UpdateError.self) { try await GitHubReleaseFetcher(version: "0.1.18", url: file).releases() }
        #expect(ReleaseFeed.url.absoluteString == "https://api.github.com/repos/sbaillar/darkslide/releases?per_page=20")
        #expect(GitHubReleaseFetcher(version: "1.2.3").version == "1.2.3")
    }

    @Test func sheetTexts() throws {
        let rel = try #require(try ReleaseFeed.releases(from: Self.feedJSON()).last)
        let current = "0.1.17"
        #expect(UpdateText.title(.checking) == "Checking for updates…")
        #expect(UpdateText.detail(.checking, current: current, beta: false) == "")
        #expect(UpdateText.detail(.upToDate, current: current, beta: true) == "Darkslide 0.1.17 is the newest version, betas included.")
        #expect(UpdateText.detail(.available(rel), current: current, beta: false) == "You have 0.1.17.")
        #expect(UpdateText.detail(.downloading(rel, 0.426), current: current, beta: false) == "43%")
        #expect(UpdateText.title(.verifying(rel)) == "Checking Darkslide 0.1.18…")
        #expect(UpdateText.detail(.verifying(rel), current: current, beta: false) == "Checking its fingerprint and signature.")
        #expect(UpdateText.title(.ready(rel)) == "Darkslide 0.1.18 is ready to install")
        #expect(UpdateText.detail(.ready(rel), current: current, beta: false).hasPrefix("It installs when you quit Darkslide."))
        #expect(UpdateText.detail(.failed("Nope."), current: current, beta: false) == "Nope.")
    }
}
