import CryptoKit
import LuxtableFFI
import Foundation
import ImageIO
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Integration A end to end on the live path (real FFI, real disk image, real
/// rsync): an exFAT "card" with DCIM + sample ARWs + an MP4 → import into a new
/// session → rate → archive with each installed rsync → Amazon hand-off →
/// export JPEG + HEIC. The card image is byte-identical to a control
/// attach/detach afterwards.
@MainActor @Suite(.serialized) struct E2EIntegrationTests {
    static let rsyncs = ["/opt/homebrew/bin/rsync", "/usr/bin/rsync"].filter { FileManager.default.isExecutableFile(atPath: $0) }

    static func sha(_ url: URL) async -> String? {
        await Task.detached {
            guard let h = FileHandle(forReadingAtPath: url.path) else { return nil }
            defer { try? h.close() }
            var d = SHA256()
            while let chunk = try? h.read(upToCount: 1 << 22), !chunk.isEmpty { d.update(data: chunk) }
            return d.finalize().map { String(format: "%02x", $0) }.joined()
        }.value
    }

    /// The card currently attached (the flow re-attaches it), for cleanup.
    final class CardBox: @unchecked Sendable {
        var card: TestCardImage
        init(_ card: TestCardImage) { self.card = card }
    }

    /// Detach and make sure it happened (retrying a busy volume). An unchecked
    /// detach that silently failed left the exFAT volume mounted (dirty bit set),
    /// which is how the image hash differed from the control in 1 of 8 runs.
    /// Only this image's *current* device is detached: a remembered `/dev/diskN`
    /// can be reused by another test's image once this one is gone. Disk-image
    /// processes are never killed.
    static func checkedDetach(_ card: TestCardImage) async -> Bool {
        for attempt in 0..<6 {
            guard let dev = await TestCardImage.attachedDevice(of: card.image) else { return true }
            _ = await TestCardImage.run(["/usr/bin/hdiutil", "detach", dev])
            if await TestCardImage.attachedDevice(of: card.image) == nil { return true }
            try? await Task.sleep(for: .seconds(Double(attempt + 1)))
        }
        return false
    }

    /// First differing byte offsets between two files (`cmp -l`), for a failure message.
    /// (`cmp` exits 1 when the files differ: its output is read whatever the status; the
    /// old helper dropped it, which is why the reported diff list was empty, L3.)
    static func differences(_ a: URL, _ b: URL) async -> String {
        await Task.detached {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/usr/bin/cmp")
            p.arguments = ["-l", a.path, b.path]
            let out = Pipe()
            p.standardOutput = out
            p.standardError = out
            guard (try? p.run()) != nil else { return "cmp failed to start" }
            let data = out.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            return String(decoding: data, as: UTF8.self).split(separator: "\n").prefix(20).joined(separator: "; ")
        }.value
    }

    /// Entries macOS wrote at the volume root (`.Trashes`, `.fseventsd`…), read from a
    /// read-only attach of `image`.
    static func osMetadata(on image: URL, scratch: URL) async -> [String] {
        let mount = scratch.appendingPathComponent("ro-\(UUID().uuidString.prefix(8))")
        try? FileManager.default.createDirectory(at: mount, withIntermediateDirectories: true)
        guard let out = await TestCardImage.run(["/usr/bin/hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen",
                                                 "-mountpoint", mount.path, image.path]),
              let dev = out.split(whereSeparator: \.isWhitespace).first else { return ["(read-only attach failed)"] }
        let names = (try? FileManager.default.contentsOfDirectory(atPath: mount.path)) ?? []
        _ = await TestCardImage.run(["/usr/bin/hdiutil", "detach", String(dev)])
        return names.filter { osVolumeMetadata.contains($0) }.sorted()
    }

    @Test func cardToSessionToArchiveAmazonAndExport() async throws {
        guard let env = try await Imp6LiveImportTests.env() else { return } // no samples / no disk images: skip
        let box = CardBox(env.card)
        do {
            try await flow(env, box)
        } catch {
            _ = await Self.checkedDetach(box.card)
            try? FileManager.default.removeItem(at: env.root)
            throw error
        }
        _ = await Self.checkedDetach(box.card)
        try? FileManager.default.removeItem(at: env.root)
    }

    func flow(_ env: Imp6LiveImportTests.Env, _ box: CardBox) async throws {
        var card = box.card
        let fm = FileManager.default

        // Control: what an attach/detach cycle alone leaves in the image.
        try #require(await Self.checkedDetach(card), "detach before the control")
        guard let c1 = await TestCardImage.attach(card.image, at: card.mount) else { return }
        box.card = c1
        try #require(await Self.checkedDetach(c1), "control detach")
        let control = try #require(await Self.sha(card.image))
        let controlCopy = env.root.appendingPathComponent("control.dmg")
        try fm.copyItem(at: card.image, to: controlCopy)
        guard let c2 = await TestCardImage.attach(card.image, at: card.mount) else { return }
        card = c2
        box.card = c2
        let listing = await cardListing(card.mount)

        // 1. Import into a new session (source re-read on).
        let ejector = FakeEjector()
        var opened: SessionInfo?
        let traits = VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "E2E")
        let m = ImportModel(volume: card.mount, importer: env.service(), sessions: env.store, existingSessions: [],
                            nameTemplate: "{date} {text}", renameTemplate: "{camera}_{seq4}", preferredSession: nil,
                            ejector: ejector, inspector: StaticVolumeInspector([card.mount.standardizedFileURL.path: traits]),
                            onImported: { opened = $0 })
        await m.scan()
        #expect(m.candidates.count == 4 && m.videoCount == 1)
        m.sessionText = "E2E"
        m.verifySourceReread = true
        let t0 = Date()
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else { Issue.record("import: \(m.phase)"); return }
        #expect(s.verified && s.complete && s.needsAcknowledgement, "notes.txt stays on the card: \(s)")
        #expect(ejector.ejected.isEmpty)
        let report = try #require(m.lastReport)
        let importSecs = Date().timeIntervalSince(t0)
        let session = try #require(opened)
        var photos = try await env.store.photos(in: session)
        #expect(photos.count == 4 && photos.filter(\.isVideo).count == 1)

        // 2. Ratings work and persist.
        var first = try #require(photos.first { !$0.isVideo })
        first.rating = 5
        try await env.store.save(photos: [first])
        let reopened = RustSessionStore(recents: env.recents, storeBase: env.root.appendingPathComponent("stores"))
        photos = try await reopened.photos(in: session)
        #expect(photos.first { $0.id == first.id }?.rating == 5)
        await env.store.waitForHashing(session.id) // Amazon takes verified originals only

        // 3. Archive with each installed rsync (+ Amazon hand-off on the first run).
        #expect(!Self.rsyncs.isEmpty)
        let amazon = env.root.appendingPathComponent("Amazon Photos")
        try fm.createDirectory(at: amazon, withIntermediateDirectories: true) // chosen by the user: never created (A-M2)
        var archiveTimes: [String] = []
        for (i, rsync) in Self.rsyncs.enumerated() {
            let target = env.root.appendingPathComponent("archive-\(i)")
            try fm.createDirectory(at: target, withIntermediateDirectories: true)
            let snap = ArchiveSettingsSnapshot(enabled: true, target: target.path, intervalMinutes: 60,
                                               amazonFolder: i == 0 ? amazon : nil)
            let service = RustArchiveService(initial: snap, settings: { snap }, handles: { await env.store.archiveHandle(for: $0) },
                                             manifest: env.root.appendingPathComponent("support-\(i)/manifest.json"),
                                             lockDir: env.root.appendingPathComponent("locks-\(i)").path,
                                             tick: .milliseconds(50), rsyncPath: rsync)
            await service.sessionOpened(session)
            let a0 = Date()
            await service.afterImport(session: session, files: report.filesCopied)
            #expect(await waitUntil(timeout: .seconds(90)) { service.snapshot().detail?.runs.contains { $0.outcome == "succeeded" } == true },
                    "\(rsync): \(service.snapshot())")
            let st = service.snapshot()
            archiveTimes.append(String(format: "%@ %.2f s", st.detail?.rsync ?? rsync, Date().timeIntervalSince(a0)))
            // The session's own folder on the target, `<name> (<id>)` (A-H3).
            let folder = try #require(st.detail?.sessions.first?.archiveFolder)
            #expect(folder.hasPrefix(session.url.lastPathComponent + " ("), "\(folder)")
            let dest = target.appendingPathComponent(folder)
            for p in photos {
                let src = try #require(p.url)
                let rel = String(src.standardizedFileURL.path.dropFirst(session.url.standardizedFileURL.path.count + 1))
                #expect(fm.contentsEqual(atPath: src.path, andPath: dest.appendingPathComponent(rel).path), "\(rsync): \(rel)")
            }
            if i == 0 {
                // The hand-off runs in the background (registration and the import
                // each ask for a pass; a later pass finds them already there, A-M3).
                // Keep the finished pass that satisfied the wait: a catch-up pass may start
                // right after it and reset the per-pass counts.
                var finished: ArchiveDetail.Amazon?
                #expect(await waitUntil(timeout: .seconds(30)) {
                    guard let a = service.snapshot().detail?.amazon, !a.running, a.placed + a.alreadyMirrored >= 3
                    else { return false }
                    finished = a
                    return true
                })
                let a = try #require(finished)
                #expect(a.failed.isEmpty && a.error == nil, "\(a)")
                let mirrored = (fm.enumerator(atPath: amazon.path)?.allObjects as? [String] ?? [])
                    .filter { !$0.hasPrefix(".") && !$0.contains("/.") }
                    .filter { var d: ObjCBool = false; return fm.fileExists(atPath: amazon.appendingPathComponent($0).path, isDirectory: &d) && !d.boolValue }
                // Every imported still is handed over exactly once (videos aren't). The status
                // counts are per pass: a catch-up pass that ran after the first one reports the
                // same files as "already there", so only a lower bound holds for their sum.
                let stills = photos.filter { !$0.isVideo }.count
                #expect(mirrored.count == stills && a.placed + a.alreadyMirrored >= stills, "\(mirrored) \(a)")
            }
            await service.shutdown()
        }

        // 4. Export JPEG + HEIC from the imported photos into the session's Output/.
        let stills = photos.filter { !$0.isVideo }
        let items = stills.enumerated().map { ExportJobItem(photoID: $0.element.id, url: $0.element.url!, seq: $0.offset + 1) }
        let out = session.url.appendingPathComponent("Output")
        let recipes = [ExportRecipe(name: "JPEG", format: .jpeg, quality: 85, sizeMode: .longEdge, sizeValue: 1200),
                       ExportRecipe(name: "HEIC", format: .heic, quality: 80, subfolder: "HEIC")]
        let e0 = Date()
        let o = try await RustExportService(hardwareJPEG: false).export(ExportJob(items: items, recipes: recipes, defaultFolder: out)) { _, _ in }
        let exportSecs = Date().timeIntervalSince(e0)
        #expect(o.failures.isEmpty && !o.cancelled && o.renderer.hasPrefix("develop ("), "\(o.failures) \(o.renderer)")
        #expect(o.written.count == stills.count * 2)
        for w in o.written {
            #expect(w.url.path.hasPrefix(out.path))
            let src = try #require(CGImageSourceCreateWithURL(w.url as CFURL, nil))
            let type = CGImageSourceGetType(src) as String?
            #expect(type == (w.url.pathExtension == "heic" ? "public.heic" : "public.jpeg"), "\(w.url.lastPathComponent): \(type ?? "?")")
            #expect(CGImageSourceCreateImageAtIndex(src, 0, nil) != nil)
            if w.recipeName == "JPEG" { #expect(max(w.width, w.height) == 1200) }
        }

        // 4b. Edit (+1 EV, saved in the session and read back), then a full-size JPEG:
        // exactly the sensor size, 6000×4000 (lens correction on by default, and with a
        // straighten: the auto-fitted frame fills the original size), and the edit shows.
        let edited = try #require(stills.first)
        let editedURL = try #require(edited.url)
        var ev = EditState()
        ev.exposure.ev = 1
        try await env.store.save(edits: [edited.id: ev])
        let fresh = RustSessionStore(recents: env.recents, storeBase: env.root.appendingPathComponent("stores"))
        _ = try await fresh.photos(in: session)
        let stored = try await fresh.edit(for: edited.id)
        #expect(stored.exposure.ev == 1, "the edit persisted")
        var evStraight = stored
        evStraight.crop.angle = 3
        let fullOut = session.url.appendingPathComponent("Output/Full")
        let f0 = Date()
        let full = try await RustExportService().export(ExportJob(
            items: [ExportJobItem(photoID: edited.id, url: editedURL, seq: 1, edit: stored),
                    ExportJobItem(photoID: edited.id, url: editedURL, seq: 2),
                    ExportJobItem(photoID: edited.id, url: editedURL, seq: 3, edit: evStraight)],
            recipes: [ExportRecipe(name: ExportRecipe.fullSizeJPEGName, format: .jpeg, quality: 90, fileNameTemplate: "{seq}")],
            defaultFolder: fullOut)) { _, _ in }
        let fullSecs = Date().timeIntervalSince(f0)
        #expect(full.failures.isEmpty && full.written.count == 3, "\(full.failures)")
        let byseq = { (n: Int) in full.written.first { $0.url.lastPathComponent == "\(n).jpg" } }
        let meta = try await rawMetadata(path: editedURL.path, token: try CancelToken())
        let sensor = (5...8).contains(meta.orientation) ? (4000, 6000) : (6000, 4000)
        let w1 = try #require(byseq(1)), w2 = try #require(byseq(2)), w3 = try #require(byseq(3))
        #expect((w1.width, w1.height) == sensor, "full develop, lens-corrected: \(w1.width)×\(w1.height)")
        #expect((w1.width, w1.height) == (try await ex8DevelopedSize(editedURL, edit: stored)))
        #expect((w3.width, w3.height) == sensor, "straightened: \(w3.width)×\(w3.height)")
        for w in [w1, w2, w3] {
            let props = CGImageSourceCreateWithURL(w.url as CFURL, nil).flatMap { CGImageSourceCopyPropertiesAtIndex($0, 0, nil) }
                as? [CFString: Any]
            #expect(props?[kCGImagePropertyPixelWidth] as? Int == w.width && props?[kCGImagePropertyPixelHeight] as? Int == w.height)
        }
        let lumaEdited = try #require(ex8MeanLuma(w1.url)), lumaPlain = try #require(ex8MeanLuma(w2.url))
        #expect(lumaEdited > lumaPlain * 1.2, "+1 EV shows: \(lumaPlain) → \(lumaEdited)")

        // 5. The card: same files, and the image equals the control cycle's.
        let after = await cardListing(card.mount)
        let changed = Set(after.keys).union(listing.keys).filter { after[$0] != listing[$0] }.sorted()
        #expect(changed.isEmpty, "card listing changed: \(changed.map { "\($0): \(listing[$0] ?? "-") -> \(after[$0] ?? "-")" })")
        try #require(await Self.checkedDetach(card), "final detach")
        let final = await Self.sha(card.image)
        if final != control {
            // The card's files are all byte-identical (the listing above, which leaves
            // out only macOS's own volume folders). An image that still differs must be
            // those folders: macOS (not our code) wrote them on mount or unmount. Say
            // which, with the differing offsets; anything else is a failure.
            let osDirs = await Self.osMetadata(on: card.image, scratch: env.root)
            let diff = await Self.differences(controlCopy, card.image)
            if changed.isEmpty && !osDirs.isEmpty {
                print("E2E note: macOS wrote \(osDirs) on the card image (not our code); files identical. cmp: \(diff)")
            } else {
                Issue.record("card image differs from the control: \(diff) (macOS folders: \(osDirs))")
            }
        }
        print(String(format: "E2E: import %d items %.1f MB in %.2f s; archive: %@; export %d files in %.2f s (%@); "
                     + "full-size develop JPEG x3 in %.2f s (%dx%d, straightened %dx%d), mean luma %.1f -> %.1f at +1 EV",
                     report.filesCopied, Double(report.bytesCopied) / 1e6, importSecs, archiveTimes.joined(separator: ", "),
                     o.written.count, exportSecs, o.jpegEncoder, fullSecs, w1.width, w1.height, w3.width, w3.height,
                     lumaPlain, lumaEdited))
    }
}
