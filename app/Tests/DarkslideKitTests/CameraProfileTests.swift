import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideKit

/// A minimal little-endian DNG camera profile (`IIRC`): name, camera, one colour matrix.
private func syntheticDCP(name: String, camera: String = "Sony ILCE-9") -> Data {
    var tags: [(UInt16, UInt16, UInt32, Data)] = []
    func ascii(_ s: String) -> Data { Data(s.utf8) + Data([0]) }
    func srational(_ v: [Double]) -> Data {
        var d = Data()
        for x in v {
            var n = Int32((x * 10000).rounded()).littleEndian
            var den = Int32(10000).littleEndian
            d.append(Data(bytes: &n, count: 4))
            d.append(Data(bytes: &den, count: 4))
        }
        return d
    }
    tags.append((50708, 2, 0, ascii(camera)))
    tags.append((50721, 10, 9, srational([0.75, -0.2, -0.05, -0.4, 1.2, 0.2, -0.05, 0.15, 0.6])))
    tags.append((50936, 2, 0, ascii(name)))
    var out = Data("IIRC".utf8)
    func u16(_ v: UInt16) -> Data { var x = v.littleEndian; return Data(bytes: &x, count: 2) }
    func u32(_ v: UInt32) -> Data { var x = v.littleEndian; return Data(bytes: &x, count: 4) }
    out.append(u32(8))
    let ifdLen = 2 + 12 * tags.count + 4
    var payload = Data()
    var entries = Data()
    for (tag, type, count, bytes) in tags {
        let n = type == 2 ? UInt32(bytes.count) : count
        entries.append(u16(tag) + u16(type) + u32(n))
        if bytes.count <= 4 {
            entries.append(bytes + Data(count: 4 - bytes.count))
        } else {
            entries.append(u32(UInt32(8 + ifdLen + payload.count)))
            payload.append(bytes)
            if payload.count % 2 == 1 { payload.append(0) }
        }
    }
    out.append(u16(UInt16(tags.count)))
    out.append(entries)
    out.append(u32(0))
    out.append(payload)
    return out
}

@Suite struct CameraProfileTests {
    @Test func jsonRoundTripAndLegacyRule() {
        for p in [CameraProfileRef.standard, .neutral, .vivid, .dcp(hash: "ab", name: "Adobe Standard")] {
            #expect(CameraProfileRef(json: p.json) == p)
        }
        // No key (a sidecar from before profiles) or not an object: the legacy Neutral look.
        #expect(CameraProfileRef(json: nil) == .neutral)
        #expect(CameraProfileRef(json: "vivid") == .neutral)
        // A kind this build can't read is kept verbatim (L6), never downgraded.
        #expect(CameraProfileRef(json: ["kind": "future", "grain": 0.5]) == .other(json: #"{"grain":0.5,"kind":"future"}"#))
        #expect(CameraProfileRef(json: ["kind": "dcp"]) == .other(json: #"{"kind":"dcp"}"#))
        #expect(CameraProfileRef.other(json: "{}").displayName == "Unknown Profile")
        #expect(!CameraProfileLibrary.isAvailable(.other(json: "{}")))
        #expect(!CameraProfileLibrary.fits(.other(json: "{}"), camera: "Sony ILCE-9"))
        #expect(CameraProfileLibrary.fits(.vivid, camera: "Anything"))
        #expect(CameraProfileRef(json: ["kind": "dcp", "hash": "h"]) == .dcp(hash: "h", name: "Camera Profile"))
        #expect(CameraProfileRef.vivid.displayName == "Darkslide Vivid")
        #expect(CameraProfileRef.dcp(hash: "h", name: "X").displayName == "X")
    }

    @Test func newEditsAreStandardOldOnesNeutralAndBothUnedited() throws {
        #expect(EditState().profile == .standard)
        let (old, _) = try AdjustmentsCodec.decode(#"{"exposure":{"exposure_ev":0}}"#)
        #expect(old.profile == .neutral)
        #expect(old.isIdentity)
        var vivid = EditState()
        vivid.profile = .vivid
        #expect(!vivid.isIdentity)
        // The codec always writes the profile, even onto an empty base.
        let json = try AdjustmentsCodec.encode(vivid)
        #expect(json.contains(#""profile":{"kind":"vivid"}"#))
        #expect(try AdjustmentsCodec.decode(json).0.profile == .vivid)
    }

    @Test func rustDefaultsAndSidecarRulesAgree() throws {
        // The Rust default edit is Standard; Rust normalizing an old edit keeps Neutral.
        let (d, _) = try AdjustmentsCodec.decode(try defaultAdjustmentsJson())
        #expect(d.profile == .standard)
        let norm = try normalizeAdjustmentsJson(json: #"{"exposure":{"exposure_ev":1}}"#)
        #expect(try AdjustmentsCodec.decode(norm).0.profile == .neutral)
    }

    @Test func profileIsACopyGroup() {
        var src = EditState()
        src.profile = .vivid
        #expect(AdjustGroup.profile.differs(src, EditState()))
        #expect(AdjustGroup.changed(in: src) == [.profile])
        let only = PasteOptions(groups: [.profile])
        #expect(AdjustmentClipboard(source: src).apply(to: EditState(), options: only).profile == .vivid)
        let other = PasteOptions(groups: [.exposure])
        #expect(AdjustmentClipboard(source: src).apply(to: EditState(), options: other).profile == .standard)
        #expect(AdjustGroup.profile.title == "Camera Profile" && !AdjustGroup.profile.supportsRelative)
        #expect(AdjustGroup(rust: AdjustGroup.profile.rust) == .profile)
        #expect(RustCore.groups.first?.0 == .profile)
    }

    @Test func unknownKindsRoundTripThroughTheCodecAndRust() throws {
        let sidecar = #"{"profile":{"kind":"film","stock":"Portra 400"},"exposure":{"exposure_ev":0.5}}"#
        var (e, raw) = try AdjustmentsCodec.decode(sidecar)
        guard case .other = e.profile else {
            Issue.record("not kept: \(e.profile)")
            return
        }
        #expect(!e.isIdentity && !e.isDefault)
        e.exposure.ev = 1
        let saved = try AdjustmentsCodec.encode(e, onto: raw)
        let profile = try #require(try JSONSerialization.jsonObject(with: Data(saved.utf8)) as? [String: Any])["profile"]
        #expect((profile as? [String: String]) == ["kind": "film", "stock": "Portra 400"])
        // Rust keeps it too (its normalized JSON still has it).
        let norm = try normalizeAdjustmentsJson(json: saved)
        (e, raw) = try AdjustmentsCodec.decode(norm)
        #expect(e.profile == .other(json: #"{"kind":"film","stock":"Portra 400"}"#) && e.exposure.ev == 1)
    }

    /// H1: Neutral on an otherwise default edit is an edit to keep (it renders unlike
    /// the default, Standard), though not one that badges the photo.
    @MainActor @Test func neutralIsKeptOnAnUneditedPhoto() async throws {
        let store = MockSessionStore(counts: [6])
        let m = makeModel(store: store)
        await m.start()
        let id = try #require(m.visibleIDs.first)
        m.focus(id, extend: false)
        m.setProfile(.neutral)
        #expect(m.edit(for: id).profile == .neutral)
        #expect(m.edit(for: id).isIdentity && !m.edit(for: id).isDefault)
        #expect(m.photo(id)?.isEdited == false, "no edited badge for the legacy look")
        m.undo()
        #expect(m.edit(for: id) == EditState())
        m.redo()
        #expect(m.edit(for: id).profile == .neutral)
        // A slider moved and back keeps Neutral (the reset no longer drops the edit).
        m.updateEdit(label: "ev") { $0.exposure.ev = 1 }
        #expect(m.photo(id)?.isEdited == true)
        m.updateEdit(label: "ev") { $0.exposure.ev = 0 }
        #expect(m.edit(for: id).profile == .neutral && m.photo(id)?.isEdited == false)
        // Reload: the store still has it.
        await m.saver.flush()
        let reloaded = makeModel(store: store)
        await reloaded.start()
        #expect(reloaded.edit(for: id).profile == .neutral && reloaded.photo(id)?.isEdited == false)
    }

    /// H1: an untouched sidecar from before profiles stays Neutral (what main renders),
    /// in the model, in what the renderer is sent, and in Rust.
    @MainActor @Test func anOldSidecarStaysNeutral() async throws {
        let (old, _) = try AdjustmentsCodec.decode(#"{"exposure":{"exposure_ev":0}}"#)
        let store = MockSessionStore(counts: [4])
        let session = try #require(try await store.listSessions().first)
        let id = try #require(try await store.photos(in: session).first?.id)
        try await store.save(edits: [id: old])
        let m = makeModel(store: store)
        await m.start()
        #expect(m.edit(for: id).profile == .neutral && m.photo(id)?.isEdited == false)
        let sent = try AdjustmentsCodec.encode(m.edit(for: id))
        #expect(sent.contains(#""profile":{"kind":"neutral"}"#))
        #expect(try AdjustmentsCodec.decode(try normalizeAdjustmentsJson(json: sent)).0.profile == .neutral)
        // The next slider move commits Neutral, not Standard.
        m.focus(id, extend: false)
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.5 }
        #expect(m.edit(for: id).profile == .neutral)
        // The store returns it (the Rust store too: SessionStoreTests.aNeutralPickSurvivesReopen).
        #expect(try await store.edits(in: session)[id]?.profile == .neutral)
    }

    @Test func libraryImportsListsAndReloads() async throws {
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("ds-profiles-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: tmp) }
        try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        let src = tmp.appendingPathComponent("Camera Standard.dcp")
        try syntheticDCP(name: "Swift Camera Standard").write(to: src)
        let lib = tmp.appendingPathComponent("Profiles")
        let o = try await CameraProfileLibrary.importProfile(at: src, into: lib)
        #expect(o.name == "Swift Camera Standard" && o.camera == "Sony ILCE-9" && o.isImported)
        guard case let .dcp(hash, _) = o.profile else {
            Issue.record("not a dcp")
            return
        }
        #expect(FileManager.default.fileExists(atPath: lib.appendingPathComponent("\(hash).dcp").path))
        #expect(CameraProfileLibrary.isAvailable(o.profile) && CameraProfileLibrary.isAvailable(.standard))
        let names = CameraProfileLibrary.options().map(\.name)
        #expect(Array(names.prefix(3)) == ["Darkslide Standard", "Darkslide Neutral", "Darkslide Vivid"])
        #expect(names.contains("Swift Camera Standard"))
        #expect(CameraProfileLibrary.options(current: o.profile).count == CameraProfileLibrary.options().count)
        #expect(CameraProfileLibrary.loadInstalled(from: lib) == 1)
        #expect(CameraProfileLibrary.loadInstalled(from: tmp.appendingPathComponent("none")) == 0)
        #expect(CameraProfileLibrary.folder.lastPathComponent == "Profiles")
        // A profile that isn't installed stays listed, marked missing.
        let gone = CameraProfileRef.dcp(hash: String(repeating: "0", count: 64), name: "Gone")
        #expect(!CameraProfileLibrary.isAvailable(gone))
        #expect(CameraProfileLibrary.options(current: gone).last?.name == "Gone (missing)")
        // A file that isn't a profile is refused.
        let bad = tmp.appendingPathComponent("bad.dcp")
        try Data("nope".utf8).write(to: bad)
        await #expect(throws: (any Error).self) { try await CameraProfileLibrary.importProfile(at: bad, into: lib) }
        #expect(CameraProfileRef(entry: CameraProfileEntry(kind: "dcp", hash: nil, name: "x", camera: nil)) == nil)
        #expect(CameraProfileRef(entry: CameraProfileEntry(kind: "odd", hash: nil, name: "x", camera: nil)) == nil)
        #expect(CameraProfileRef(entry: CameraProfileEntry(kind: "neutral", hash: nil, name: "x", camera: nil)) == .neutral)
    }

    /// M2 + L5 + L7: imported profiles load off the main thread before the first
    /// session; one made for another camera is flagged in the thumbnail key, can't be
    /// picked silently and warns on paste.
    @MainActor @Test func profilesForAnotherCamera() async throws {
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("ds-profiles-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: tmp) }
        try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
        let mine = tmp.appendingPathComponent("a9.dcp")
        let theirs = tmp.appendingPathComponent("r5.dcp")
        try syntheticDCP(name: "A9 Look \(UUID().uuidString)").write(to: mine)
        try syntheticDCP(name: "R5 Look \(UUID().uuidString)", camera: "Canon EOS R5").write(to: theirs)
        let lib = tmp.appendingPathComponent("Profiles")
        let a9 = try await CameraProfileLibrary.importProfile(at: mine, into: lib).profile
        let r5 = try await CameraProfileLibrary.importProfile(at: theirs, into: lib).profile
        #expect(CameraProfileLibrary.fits(a9, camera: "Sony ILCE-9") && !CameraProfileLibrary.fits(r5, camera: "Sony ILCE-9"))
        #expect(CameraProfileLibrary.fits(r5, camera: "Canon EOS R5"))
        // Launch loading runs in the background; the model waits for it.
        CameraProfileLibrary.startLoading(from: lib)
        #expect(await CameraProfileLibrary.loading?.value == 2)

        let m = makeModel(counts: [8])
        m.tools = FakeMatchTools()
        await m.start()
        let ids = Array(m.visibleIDs.filter { m.photo($0)?.isVideo == false }.prefix(3))
        m.focus(ids[0], extend: false)
        m.setProfile(r5)
        #expect(m.toast?.message.hasPrefix("R5 Look") == true && m.toast?.message.contains("1 photo shown with Darkslide Standard") == true)
        let p = try #require(m.photo(ids[0]))
        let misfit = m.thumbnails.fingerprint(p)
        m.copyAdjustments()
        #expect(m.applyClipboard(to: [ids[1], ids[2]], options: PasteOptions(groups: [.profile])) == 2)
        #expect(m.toast?.message == "Applied to 2 photos; the camera profile is for another camera on 2 photos (shown with Darkslide Standard)")
        m.setProfile(a9)
        #expect(m.toast?.message.hasPrefix("A9 Look") != true, "no warning when it fits")
        #expect(m.profileMisfits(in: ids) == 2)
        // The thumbnail key follows whether the profile renders: the same edit on a
        // photo from the profile's camera keys differently.
        var other = p
        other.meta.camera = "Canon EOS R5"
        #expect(m.thumbnails.fingerprint(p) != misfit)
        m.setProfile(r5)
        #expect(m.thumbnails.fingerprint(p) == misfit && m.thumbnails.fingerprint(other) != misfit)
        m.focus(ids[1], extend: false)
        m.copyFromPrevious()
        #expect(m.toast?.message.contains("another camera on 1 photo") == true)
    }
}
