import CoreGraphics
import CryptoKit
import LuxtableFFI
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// `EditState` ↔ lt-core JSON, checked against the real Rust schema via the FFI.
@Suite struct AdjustmentsCodecTests {
    func full() -> EditState {
        var e = EditState()
        e.whiteBalance.asShot = false
        e.whiteBalance.temperature = 6100
        e.whiteBalance.tint = 4
        e.exposure.ev = 0.5
        e.exposure.contrast = 12
        e.exposure.brightness = -3
        e.exposure.saturation = 18
        e.hdr.highlights = -30
        e.hdr.shadows = 24
        e.hdr.whites = 5
        e.hdr.blacks = -5
        e.levels.inBlack = 0.25
        e.levels.gamma = 1.5
        e.levels.outWhite = 0.75
        e.curve.points[.rgb] = [CurvePoint(0.25, 0.125), CurvePoint(0.75, 0.875)]
        e.curve.points[.blue] = [CurvePoint(0.5, 0.25)]
        e.colorBalance.shadows = WheelValue(hue: 200, saturation: 0.25, lightness: -0.125)
        e.colorBalance.master = WheelValue(hue: 30, saturation: 0.5, lightness: 0)
        e.colorEditor.bands[2].saturation = 20
        e.colorEditor.bands[5].hueShift = -10
        e.detail.clarity = 15
        e.detail.sharpenAmount = 200
        e.noise.luminance = 40
        e.vignette.amountEV = -0.5
        e.lens.enabled = false
        e.lens.distortion = 0.5
        e.crop.aspect = .r4x5
        e.crop.angle = 2.5
        e.crop.quarterTurns = 1
        e.crop.rect = CGRect(x: 0.125, y: 0.25, width: 0.5, height: 0.5)
        let brush = BrushStroke(points: [CGPoint(x: 0.25, y: 0.5), CGPoint(x: 0.5, y: 0.5)], size: 0.0625, hardness: 0.5,
                                flow: 1, erase: false)
        e.layers = [
            LayerEdit(id: 3, name: "Sky", enabled: true, opacity: 0.75, exposureEV: -0.5,
                      mask: MaskEdit(shapes: [.linear(.add, start: CGPoint(x: 0.5, y: 0), end: CGPoint(x: 0.5, y: 0.5)),
                                              .brush(.subtract, [brush])], invert: true, feather: 0.0375)),
            LayerEdit(id: 7, name: "Face", mask: MaskEdit(shapes: [
                .radial(.add, center: CGPoint(x: 0.5, y: 0.5), radius: CGSize(width: 0.25, height: 0.125), angle: 30, feather: 0.5),
                .ai(.intersect, .person), .fill(.add),
            ])),
        ]
        return e
    }

    func rustDefault() throws -> AdjustmentsCodec.JSON {
        try AdjustmentsCodec.decode(try defaultAdjustmentsJson()).1
    }

    @Test func rustDefaultsDecodeToIdentity() throws {
        let (e, _) = try AdjustmentsCodec.decode(try defaultAdjustmentsJson())
        #expect(e == EditState())
        #expect(e.isIdentity)
    }

    /// `full()` with values that aren't binary fractions (0.1, 5123.7, …), so
    /// a round trip through Rust's `f32` can only be checked with a tolerance (R3-L4).
    func fullDecimal() -> EditState {
        var e = full()
        e.whiteBalance.temperature = 5123.7
        e.whiteBalance.tint = 3.3
        e.exposure.ev = 0.1
        e.exposure.contrast = 12.3
        e.exposure.brightness = -3.7
        e.exposure.saturation = 18.9
        e.hdr.highlights = -30.1
        e.hdr.shadows = 24.6
        e.levels.inBlack = 0.1
        e.levels.gamma = 1.3
        e.levels.outWhite = 0.7
        e.curve.points[.rgb] = [CurvePoint(0.1, 0.2), CurvePoint(0.7, 0.9)]
        e.colorBalance.shadows = WheelValue(hue: 200.3, saturation: 0.3, lightness: -0.1)
        e.colorEditor.bands[2].saturation = 20.7
        e.colorEditor.bands[5].hueShift = -10.3
        e.detail.clarity = 15.1
        e.detail.sharpenAmount = 200.3
        e.noise.luminance = 40.7
        e.vignette.amountEV = -0.3
        e.lens.distortion = 0.3
        e.crop.angle = 2.3
        e.crop.rect = CGRect(x: 0.1, y: 0.2, width: 0.6, height: 0.7)
        e.layers[0].opacity = 0.7
        e.layers[0].exposureEV = -0.3
        e.layers[0].mask.feather = 0.04 // within lt-core MAX_PLANE_FEATHER
        e.layers[0].mask.shapes = [
            .linear(.add, start: CGPoint(x: 0.3, y: 0.1), end: CGPoint(x: 0.3, y: 0.6)),
            .brush(.subtract, [BrushStroke(points: [CGPoint(x: 0.1, y: 0.2), CGPoint(x: 0.3, y: 0.4)], size: 0.07,
                                           hardness: 0.3, flow: 0.9, erase: true)]),
        ]
        e.layers[1].mask.shapes[0] = .radial(.add, center: CGPoint(x: 0.3, y: 0.6), radius: CGSize(width: 0.3, height: 0.1),
                                             angle: 30.3, feather: 0.3)
        return e
    }

    /// Paths where `a` and `b` differ: numbers beyond `tol` (relative above 1),
    /// other scalars by equality, containers recursively.
    func differences(_ a: Any?, _ b: Any?, tol: Double = 1e-4, at path: String = "") -> [String] {
        switch (a, b) {
        case let (x as AdjustmentsCodec.JSON, y as AdjustmentsCodec.JSON):
            return Set(x.keys).union(y.keys).sorted().flatMap { differences(x[$0], y[$0], tol: tol, at: "\(path).\($0)") }
        case let (x as [Any], y as [Any]):
            guard x.count == y.count else { return ["\(path): \(x.count) vs \(y.count) items"] }
            return zip(x, y).enumerated().flatMap { differences($1.0, $1.1, tol: tol, at: "\(path)[\($0)]") }
        default:
            if let x = AdjustmentsCodec.num(a), let y = AdjustmentsCodec.num(b) {
                return abs(x - y) <= tol * max(1, abs(x)) ? [] : ["\(path): \(x) vs \(y)"]
            }
            if let x = a as? Bool, let y = b as? Bool { return x == y ? [] : ["\(path): \(x) vs \(y)"] }
            if let x = a as? String, let y = b as? String { return x == y ? [] : ["\(path): \(x) vs \(y)"] }
            if a is NSNull && b is NSNull { return [] }
            return ["\(path): \(String(describing: a)) vs \(String(describing: b))"]
        }
    }

    @Test func everyModelledFieldSurvivesRustWithinTolerance() throws {
        for e in [full(), fullDecimal()] {
            let base = try rustDefault()
            let sent = AdjustmentsCodec.overlay(e, onto: base)
            // Through lt_core::Adjustments (f32, sanitize): exactly what a sidecar stores.
            let (back, _) = try AdjustmentsCodec.decode(try normalizeAdjustmentsJson(json: try AdjustmentsCodec.encode(e, onto: base)))
            #expect(differences(sent, AdjustmentsCodec.overlay(back, onto: base)) == [])
            // Swift-only round trip.
            let (plain, _) = try AdjustmentsCodec.decode(try AdjustmentsCodec.encode(e, onto: base))
            #expect(differences(sent, AdjustmentsCodec.overlay(plain, onto: base), tol: 1e-12) == [])
            #expect(abs(back.exposure.ev - e.exposure.ev) < 1e-6 && abs(back.whiteBalance.temperature - e.whiteBalance.temperature) < 1e-3)
        }
        #expect(differences(["x": 1.0], ["x": 1.1]) == [".x: 1.0 vs 1.1"])
        #expect(differences([1, 2], [1]) == [": 2 vs 1 items"])
        #expect(differences("a", 1) == [": Optional(\"a\") vs Optional(1)"])
    }

    @Test func sparseSectionsDecodeToDefaults() throws {
        let json = """
        {"white_balance":{"mode":"custom"},"exposure":{},"hdr":{},"levels":{},"curve":{"rgb":[[0.5],"x"]},
         "color_balance":{},"color_editor":{"basic":[{},1]},"detail":{},"noise":{},"vignette":{},"lens":{},
         "crop":{"rect":{}},"layers":[{},{"mask":{"shapes":[{"kind":"brush","strokes":[{}]},{"kind":"linear"},
         {"kind":"radial"},{"kind":"ai","target":"nope"},{"kind":"color_range"},3]}},7]}
        """
        let (e, _) = try AdjustmentsCodec.decode(json)
        let d = EditState()
        #expect(!e.whiteBalance.asShot && e.whiteBalance.temperature == d.whiteBalance.temperature && e.whiteBalance.tint == 0)
        #expect(e.exposure == d.exposure && e.hdr == d.hdr && e.levels == d.levels && e.curve == d.curve)
        #expect(e.colorBalance == d.colorBalance && e.colorEditor == d.colorEditor)
        #expect(e.detail == d.detail && e.noise == d.noise && e.vignette == d.vignette && e.lens == d.lens)
        #expect(e.crop.rect == CGRect(x: 0, y: 0, width: 1, height: 1) && e.crop.angle == 0 && e.crop.aspect == .original)
        #expect(e.layers.count == 2)
        #expect(e.layers[0].name == "" && e.layers[0].opacity == 1 && e.layers[0].enabled && e.layers[0].mask.shapes.isEmpty)
        let stroke = BrushStroke(points: [], size: 0.05, hardness: 0.5, flow: 1, erase: false)
        #expect(e.layers[1].mask.shapes == [
            .brush(.add, [stroke]), .linear(.add, start: .zero, end: .zero),
            .radial(.add, center: .zero, radius: .zero, angle: 0, feather: 0),
        ])
        // Encoding onto nothing builds every section, with default color-editor bands.
        let built = AdjustmentsCodec.overlay(EditState(), onto: [:])
        let basic = (built["color_editor"] as? AdjustmentsCodec.JSON)?["basic"] as? [AdjustmentsCodec.JSON]
        #expect(basic?.count == 8 && basic?[0]["width"] as? Double == 45)
        #expect(try AdjustmentsCodec.decode(try AdjustmentsCodec.encode(EditState())).0.isIdentity)
    }

    @Test func fieldsTheAppDoesNotModelArePreserved() throws {
        var base = try rustDefault()
        var ce = base["color_editor"] as! AdjustmentsCodec.JSON
        ce["advanced"] = [["center_hue": 25.0, "width": 30.0, "smoothness": 0.5, "hue_shift": 2.0, "saturation": 10.0, "lightness": 0.0]]
        base["color_editor"] = ce
        var lens = base["lens"] as! AdjustmentsCodec.JSON
        lens["profile"] = "Sony FE 85mm F1.4 GM"
        lens["keystone_vertical"] = 3.0
        base["lens"] = lens
        var crop = base["crop"] as! AdjustmentsCodec.JSON
        crop["flip_h"] = true
        base["crop"] = crop
        base["layers"] = [[
            "id": 1, "name": "Dodge", "enabled": true, "opacity": 1.0,
            "adjustments": ["exposure": ["exposure_ev": 0.5], "hdr": ["shadows": 30.0], "clarity": 10.0],
            "mask": ["shapes": [["kind": "color_range", "op": "add", "range": ["center_hue": 200.0, "width": 30.0]],
                                ["kind": "fill", "op": "add"]],
                     "invert": false, "feather": 0.0, "luma_range": ["low": 0.1, "high": 0.9, "low_falloff": 0.1, "high_falloff": 0.1]],
        ]]
        let (decoded, _) = try AdjustmentsCodec.decode(String(decoding: try JSONSerialization.data(withJSONObject: base), as: UTF8.self))
        #expect(decoded.layers.first?.mask.shapes == [.fill(.add)], "unknown shape kinds are not shown")
        var e = decoded
        e.exposure.ev = 1
        e.layers[0].exposureEV = 0.25
        let out = try normalizeAdjustmentsJson(json: try AdjustmentsCodec.encode(e, onto: base))
        #expect(out.contains("\"advanced\":[{"), "color editor advanced ranges kept")
        #expect(out.contains("Sony FE 85mm F1.4 GM") && out.contains("\"keystone_vertical\":3"))
        #expect(out.contains("\"flip_h\":true"))
        #expect(out.contains("\"shadows\":30") && out.contains("\"clarity\":10"), "layer tools beyond exposure kept")
        #expect(out.contains("color_range") && out.contains("luma_range"))
        let (back, _) = try AdjustmentsCodec.decode(out)
        #expect(back.exposure.ev == 1 && back.layers[0].exposureEV == 0.25)
        // Changing the known shapes keeps the unknown one, appended.
        e.layers[0].mask.shapes = [.ai(.add, .subject)]
        let changed = try AdjustmentsCodec.encode(e, onto: base)
        let raw = try AdjustmentsCodec.decode(changed).1
        let shapes = ((raw["layers"] as! [AdjustmentsCodec.JSON])[0]["mask"] as! AdjustmentsCodec.JSON)["shapes"] as! [AdjustmentsCodec.JSON]
        #expect(shapes.map { $0["kind"] as? String } == ["ai", "color_range"])
    }

    @Test func aspectsAndLayersMapBothWays() throws {
        #expect(AdjustmentsCodec.aspect(nil) == .original)
        #expect(AdjustmentsCodec.aspect(["long": 3, "short": 2]) == .r3x2)
        #expect(AdjustmentsCodec.aspect(["long": 16, "short": 9]) == .r16x9)
        #expect(AdjustmentsCodec.aspect(["long": 4, "short": 3]) == .r4x3 && AdjustmentsCodec.aspect(["long": 8, "short": 5]) == .r8x5)
        #expect(AdjustmentsCodec.aspect(["long": 7, "short": 5]) == .r7x5)
        #expect(AdjustmentsCodec.aspect(["long": 7, "short": 3]) == .free, "unknown ratio")
        #expect(AdjustmentsCodec.aspect(["long": 1, "short": 0]) == .original)
        // An unknown aspect stays when the user leaves it alone...
        var base = try rustDefault()
        var crop = base["crop"] as! AdjustmentsCodec.JSON
        crop["aspect"] = ["long": 7, "short": 3]
        base["crop"] = crop
        let (e, _) = try AdjustmentsCodec.decode(String(decoding: try JSONSerialization.data(withJSONObject: base), as: UTF8.self))
        let kept = try AdjustmentsCodec.decode(try AdjustmentsCodec.encode(e, onto: base)).1
        #expect(((kept["crop"] as! AdjustmentsCodec.JSON)["aspect"] as? AdjustmentsCodec.JSON)?["long"] as? Int == 7)
        // ...and is replaced when they pick one.
        var picked = e
        picked.crop.aspect = .r1x1
        let replaced = try AdjustmentsCodec.decode(try AdjustmentsCodec.encode(picked, onto: base)).0
        #expect(replaced.crop.aspect == .r1x1)
        // Removing a layer drops it; quarter turns wrap.
        var f = full()
        f.layers.removeFirst()
        f.crop.quarterTurns = -1
        let (g, _) = try AdjustmentsCodec.decode(try AdjustmentsCodec.encode(f, onto: try rustDefault()))
        #expect(g.layers.map(\.id) == [7])
        #expect(g.crop.quarterTurns == 3)
    }

    @Test func rejectsNonObjects() {
        #expect(throws: AdjustmentsCodec.CodecError.notAnObject) { try AdjustmentsCodec.decode("[1]") }
        #expect(throws: AdjustmentsCodec.CodecError.self) { try AdjustmentsCodec.decode("{nope") }
        #expect(AdjustmentsCodec.num(true) == nil, "booleans are not numbers")
        #expect(AdjustmentsCodec.shape(["kind": "ai", "target": "unicorn"]) == nil)
        // No `profile` key: an edit from before profiles (the legacy Neutral look).
        var legacy = EditState()
        legacy.profile = .legacy
        #expect(AdjustmentsCodec.decode([:]) == legacy)
    }
}

/// Live path: real FFI, real lt-session, real Sony A9 samples (skips without them).
@Suite(.serialized) struct RustSessionStoreLiveTests {
    func sha(_ url: URL) throws -> String {
        SHA256.hash(data: try Data(contentsOf: url)).map { String(format: "%02x", $0) }.joined()
    }

    func recents() -> RecentFolders {
        RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!)
    }

    @Test func ratingsTagsPicksAndEditsPersistAcrossReopen() async throws {
        guard let dir = try sampleFolderCopy(limit: 8) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let files = try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
        let before = try Dictionary(uniqueKeysWithValues: files.map { ($0.lastPathComponent, try sha($0)) })
        let r = recents()
        let clock = ContinuousClock()

        let store = RustSessionStore(recents: r)
        #expect(store.persistsChanges)
        var t = clock.now
        let info = try await store.add(folder: dir)
        let openMs = (clock.now - t).inMilliseconds
        let photos = try await store.photos(in: info)
        #expect(photos.count == files.count)
        #expect(photos.allSatisfy { $0.url != nil && $0.meta.camera.contains("ILCE-9") && $0.meta.pixelWidth > 0 })
        #expect(photos.contains { $0.meta.shotNumber != nil } && photos.allSatisfy { $0.meta.fileSequence != nil })
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent(".lt/session/.lt/settings/session.json").path))

        var a = photos[0], b = photos[1], c = photos[2]
        a.rating = 4
        b.colorTag = .green
        c.pick = .rejected
        t = clock.now
        try await store.save(photos: [a])
        let rateMs = (clock.now - t).inMilliseconds
        try await store.save(photos: [b, c])
        var edit = EditState()
        edit.exposure.ev = 1.25
        edit.crop.rect = CGRect(x: 0.125, y: 0.125, width: 0.5, height: 0.5)
        edit.layers = [LayerEdit(id: 0, name: "Sky", mask: MaskEdit(shapes: [.ai(.add, .subject)]))]
        try await store.save(edits: [a.id: edit])
        #expect(try await store.edit(for: a.id) == edit)

        // A new store (fresh process state) sees everything, from the sidecars.
        let again = RustSessionStore(recents: r)
        let listed = try await again.listSessions()
        #expect(listed.map(\.url.path) == [dir.resolvingSymlinksInPath().path])
        t = clock.now
        let reopened = try await again.photos(in: listed[0])
        let reopenMs = (clock.now - t).inMilliseconds
        #expect(reopened.map(\.id) == photos.map(\.id), "ids are stable across reopen")
        #expect(reopened.first { $0.id == a.id }?.rating == 4)
        #expect(reopened.first { $0.id == b.id }?.colorTag == .green)
        #expect(reopened.first { $0.id == c.id }?.pick == .rejected)
        #expect(reopened.first { $0.id == a.id }?.isEdited == true)
        let edits = try await again.edits(in: listed[0])
        #expect(edits == [a.id: edit])
        #expect(try await again.edit(for: a.id) == edit)

        // The user's files were never modified.
        for f in files { #expect(try sha(f) == before[f.lastPathComponent]) }
        print("SESSION live: open-in-place \(files.count) samples \(String(format: "%.1f", openMs)) ms, reopen+list \(String(format: "%.1f", reopenMs)) ms, one rating write \(String(format: "%.2f", rateMs)) ms")
    }

    /// H1 through the real Rust store: Neutral on an otherwise default edit is loaded
    /// back on reopen (it isn't the default), without the edited badge.
    @MainActor @Test func aNeutralPickSurvivesReopen() async throws {
        guard let dir = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = recents()
        let store = RustSessionStore(recents: r)
        let info = try await store.add(folder: dir)
        let photos = try await store.photos(in: info)
        var neutral = EditState()
        neutral.profile = .neutral
        try await store.save(edits: [photos[0].id: neutral])

        let again = RustSessionStore(recents: r)
        let listed = try await again.listSessions()
        let reopened = try await again.photos(in: listed[0])
        #expect(try await again.edits(in: listed[0]) == [photos[0].id: neutral])
        #expect(reopened.allSatisfy { !$0.isEdited })
        let m = makeModel(store: again)
        await m.start()
        #expect(m.edit(for: photos[0].id).profile == .neutral && m.edit(for: photos[1].id).profile == .standard)
    }

    @Test func newFilesAreAddedOnReopen() async throws {
        let all = sampleARWs()
        guard all.count >= 3, let dir = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = recents()
        let s1 = RustSessionStore(recents: r)
        let info = try await s1.add(folder: dir)
        #expect(try await s1.photos(in: info).count == 2)
        let extra = all[2]
        try FileManager.default.copyItem(at: extra, to: dir.appendingPathComponent(extra.lastPathComponent))
        let s2 = RustSessionStore(recents: r)
        let info2 = try await s2.add(folder: dir)
        #expect(info2.photoCount == 3)
        #expect(try await s2.photos(in: info2).count == 3)
        #expect(await s2.lastFailures.isEmpty)
    }

    @Test func modelRatingSurvivesRelaunchThroughTheStore() async throws {
        guard let dir = try sampleFolderCopy(limit: 3) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = recents()
        let s1 = RustSessionStore(recents: r)
        let info = try await s1.add(folder: dir)
        let m1 = await MainActor.run { () -> AppModel in
            let settings = makeSettings()
            settings.onboardingComplete = true
            settings.autoAdvance = false
            let m = AppModel(sessions: s1, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                             importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                             ai: MockAIService(), settings: settings, mode: .live)
            m.watchCards = false
            return m
        }
        await m1.open(info)
        let id = try #require(await m1.focusedID)
        await m1.perform(.rate(5))
        await m1.saver.flush()
        let s2 = RustSessionStore(recents: r)
        let listed = try await s2.listSessions()
        let photos = try await s2.photos(in: listed[0])
        #expect(photos.first { $0.id == id }?.rating == 5)
    }
}

@Suite struct RustSessionStoreUnitTests {
    @Test func writePlanOnlyWritesChangedFields() {
        let a = samplePhoto(1), b = samplePhoto(2)
        var a2 = a, b2 = b
        a2.rating = 3
        b2.rating = 3
        b2.colorTag = .red
        var c = samplePhoto(3)
        c.pick = .picked
        let plan = RustSessionStore.writePlan([b2, a2, c], stored: [a.id: a, b.id: b])
        #expect(plan.ratings == [3: [1, 2], 0: [3]], "unknown photos write every field")
        #expect(plan.tags == [.red: [2], .none: [3]])
        #expect(plan.picks == [.picked: [3]])
        #expect(RustSessionStore.writePlan([a], stored: [a.id: a]) == RustSessionStore.WritePlan())
    }

    @Test func labelsAndFlagsRoundTrip() {
        for t in ColorTag.allCases { #expect(RustSessionStore.tag(RustSessionStore.label(t)) == t) }
        #expect(RustSessionStore.tag(.orange) == .orange && RustSessionStore.tag(.pink) == .pink, "7 labels, lossless")
        for p in PickState.allCases { #expect(RustSessionStore.pick(RustSessionStore.flag(p)) == p) }
    }

    @Test func recordMapping() {
        let r = VariantRecord(variantId: 42, imageId: 7, originalPath: "/x/DSC07062.ARW", fileName: "DSC07062.ARW", name: "",
                              rating: 3, colorTag: .purple, pick: .picked, edited: true, readOnly: false, missing: false,
                              fileChanged: false, keptInStore: false, fileSize: 1000, fileSequence: 7062, captureTimeMs: 1_786_209_940_120, width: 6000, height: 4000,
                              meta: nil)
        let p = RustSessionStore.photo(from: r)
        #expect(p.id == PhotoID(42) && p.rating == 3 && p.colorTag == .purple && p.pick == .picked && p.isEdited)
        #expect(p.meta.pixelWidth == 6000 && p.meta.fileSequence == 7062 && p.url?.path == "/x/DSC07062.ARW")
        #expect(p.meta.captureDate == Date(timeIntervalSince1970: 1_786_209_940.12))
        #expect(!p.isMissing && !p.fileChanged)
        var gone = r
        gone.missing = true
        gone.fileChanged = true
        let q = RustSessionStore.photo(from: gone)
        #expect(q.isMissing && q.fileChanged)
    }

    @Test func progressEventsMapToOpenPhases() {
        func ev(_ kind: String, _ done: UInt64, _ total: UInt64) -> ProgressEvent {
            ProgressEvent(taskId: 1, kind: kind, phase: .running, completed: done, total: total)
        }
        #expect(ProgressRelay.progress(ev("open.scan", 0, 0)) == OpenProgress(phase: .scanning, done: 0, total: 0))
        #expect(ProgressRelay.progress(ev("open.register", 3, 9)) == OpenProgress(phase: .registering, done: 3, total: 9))
        #expect(ProgressRelay.progress(ev("hash", 9, 9)) == OpenProgress(phase: .hashing, done: 9, total: 9))
        #expect(ProgressRelay.progress(ev("open", 0, 0)) == nil)
        let seen = Locked<[OpenProgress]>([])
        let relay = ProgressRelay { p in seen.withLock { $0.append(p) } }
        relay.onProgress(event: ev("open", 0, 0))
        relay.onProgress(event: ev("hash", 1, 2))
        #expect(seen.withLock { $0 } == [OpenProgress(phase: .hashing, done: 1, total: 2)])
    }

    @Test func cardStoresAreKeyedByVolumeAndPathOnIt() throws {
        let base = URL(fileURLWithPath: "/tmp/base")
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("lt-fb-\(UUID())")
        try FileManager.default.createDirectory(at: tmp.appendingPathComponent("x"), withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: tmp) }
        let a = RustSessionStore.fallbackStore(for: tmp, base: base)
        let b = RustSessionStore.fallbackStore(for: tmp.appendingPathComponent("x"), base: base)
        #expect(a.path.hasPrefix("/tmp/base/") && a != b)
        #expect(a.deletingLastPathComponent() == b.deletingLastPathComponent(), "same volume")
        #expect(!a.path.contains("no-volume-id"))
        let gone = RustSessionStore.fallbackStore(for: tmp.appendingPathComponent("gone"), base: base)
        #expect(gone.deletingLastPathComponent().lastPathComponent == "vol-" + RustSessionStore.shortHash("unknown"))
        #expect(a.lastPathComponent.count == 24)
        #expect(RustSessionStore.defaultStoreBase == AppPaths.support.appendingPathComponent("InPlace", isDirectory: true))
    }

    @Test func autoApplyRecordCarriesGroupsAndJSON() throws {
        var e = EditState()
        e.exposure.ev = 0.5
        let p = Preset(name: "Bright", groups: [.exposure, .whiteBalance], relative: [.exposure], edit: e)
        let r = try RustSessionStore.autoApplyRecord(p)
        #expect(r.name == "Bright" && r.groups == [.exposure, .whiteBalance] && r.relative == [.exposure])
        #expect(try AdjustmentsCodec.decode(r.adjustmentsJson).0.exposure.ev == 0.5)
        for g in AdjustGroup.allCases { #expect(AdjustGroup(rust: g.rust) == g) }
    }

    @Test func notOpenedPhotosHaveNoEditsAndCreateIsRefused() async throws {
        let s = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!))
        #expect(try await s.edit(for: PhotoID(1)) == EditState())
        // A write for a photo of no open session fails loudly (never silently dropped).
        await #expect(throws: UnsavableChanges.self) { try await s.save(photos: [samplePhoto()]) }
        await #expect(throws: UnsavableChanges.self) { try await s.save(edits: [PhotoID(1): EditState()]) }
        try await s.save(photos: [])
        await #expect(throws: MockError.self) { try await s.createSession(named: "x") }
        let ghost = SessionInfo(name: "ghost", url: URL(fileURLWithPath: "/nonexistent-\(UUID())"), photoCount: 0, createdAt: Date())
        #expect(try await s.albums(in: ghost).isEmpty)
        await #expect(throws: RenderError.self) { try await s.photos(in: ghost) }
        #expect(try await s.listSessions().isEmpty)
        #expect(!RustSessionStore.defaultJSON().isEmpty)
    }

    @Test func aFolderThatIsNotThereExplains() async throws {
        let s = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!))
        do {
            _ = try await s.add(folder: URL(fileURLWithPath: "/nonexistent-\(UUID())"))
            Issue.record("expected an error")
        } catch let e as RenderError {
            #expect(e.kind == .io)
        }
        await #expect(throws: MockError.self) { try await s.locate(PhotoID(1), at: URL(fileURLWithPath: "/x")) }
        try await s.removeMissing([PhotoID(1)])
    }
}

@Suite struct ProgressStreamTerminalTests {
    @Test func terminalEventsSurviveABurst() async {
        let stream = ProgressStream(bufferingNewest: 4)
        stream.onProgress(event: ProgressEvent(taskId: 1, kind: "t", phase: .started, completed: 0, total: 1000))
        for i in 0..<1000 {
            stream.onProgress(event: ProgressEvent(taskId: 1, kind: "t", phase: .running, completed: UInt64(i), total: 1000))
        }
        stream.onProgress(event: ProgressEvent(taskId: 1, kind: "t", phase: .finished, completed: 1000, total: 1000))
        for i in 0..<50 {
            stream.onProgress(event: ProgressEvent(taskId: 2, kind: "t", phase: .running, completed: UInt64(i), total: 50))
        }
        stream.onProgress(event: ProgressEvent(taskId: 2, kind: "t", phase: .cancelled, completed: 49, total: 50))
        stream.finish()
        var got: [ProgressEvent] = []
        for await e in stream.events { got.append(e) }
        #expect(got.first?.phase == .started)
        #expect(got.contains { $0.taskId == 1 && $0.phase == .finished }, "Finished is never dropped")
        #expect(got.last?.phase == .cancelled)
        #expect(got.filter { $0.phase == .running }.count == 4)
        #expect(stream.dropped == 1046)
    }

    @Test func liveConsumerGetsEventsAndCancelEndsIteration() async {
        let stream = ProgressStream()
        let consumer = Task { () -> [TaskPhase] in
            var phases: [TaskPhase] = []
            for await e in stream.events { phases.append(e.phase) }
            return phases
        }
        try? await Task.sleep(for: .milliseconds(20))
        stream.onProgress(event: ProgressEvent(taskId: 1, kind: "t", phase: .started, completed: 0, total: 1))
        stream.onProgress(event: ProgressEvent(taskId: 1, kind: "t", phase: .finished, completed: 1, total: 1))
        try? await Task.sleep(for: .milliseconds(20))
        stream.finish()
        #expect(await consumer.value == [.started, .finished])
        let waiting = Task { () -> Int in
            var n = 0
            for await _ in ProgressStream().events { n += 1 }
            return n
        }
        try? await Task.sleep(for: .milliseconds(20))
        waiting.cancel()
        #expect(await waiting.value == 0, "a cancelled consumer stops waiting")
    }
}

/// Live path, round 4: external originals, identity, moves, cards, auto-apply.
@Suite(.serialized) struct RustSessionStoreInPlaceTests {
    func recents() -> RecentFolders {
        RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!)
    }

    func tempBase() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("lt-store-\(UUID())")
    }

    @Test func progressIsReportedAndMissingFilesAreListedThenLocated() async throws {
        guard let dir = try sampleFolderCopy(limit: 3) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let seen = Locked<[OpenProgress.Phase]>([])
        let s = RustSessionStore(recents: recents(), storeBase: tempBase())
        let info = try await s.add(folder: dir) { p in seen.withLock { $0.append(p.phase) } }
        await s.waitForHashing(info.id)
        #expect(seen.withLock { $0 }.contains(.registering) && seen.withLock { $0 }.contains(.hashing))
        let photos = try await s.photos(in: info)
        #expect(photos.count == 3 && photos.allSatisfy { !$0.isMissing })
        let victim = try #require(photos.first?.url)
        let away = FileManager.default.temporaryDirectory.appendingPathComponent("lt-away-\(UUID()).ARW")
        defer { try? FileManager.default.removeItem(at: away) }
        try FileManager.default.moveItem(at: victim, to: away)

        let s2 = RustSessionStore(recents: recents(), storeBase: tempBase())
        let info2 = try await s2.add(folder: dir)
        let listed = try await s2.photos(in: info2)
        let missing = try #require(listed.first { $0.isMissing })
        #expect(missing.id == photos[0].id && listed.count == 3)
        // Locate: a different photo is refused; the moved file is accepted.
        await #expect(throws: RenderError.self) { try await s2.locate(missing.id, at: try #require(listed[1].url)) }
        let back = dir.appendingPathComponent("found-again.ARW")
        try FileManager.default.moveItem(at: away, to: back)
        try await s2.locate(missing.id, at: back)
        let after = try await s2.photos(in: info2)
        #expect(after.first { $0.id == missing.id }.map { !$0.isMissing && $0.url?.lastPathComponent == "found-again.ARW" } == true)
        // Remove missing: forgotten by the session, the file itself untouched.
        try FileManager.default.moveItem(at: back, to: away)
        _ = try await s2.photos(in: try await s2.add(folder: dir))
        try await s2.removeMissing([missing.id])
        #expect(try await s2.photos(in: info2).count == 2)
        #expect(FileManager.default.fileExists(atPath: away.path))
    }

    @Test func aFolderRenamedWhileOpenIsFollowed() async throws {
        guard let src = try sampleFolderCopy(limit: 2) else { return }
        let parent = src.deletingLastPathComponent().appendingPathComponent("lt-move-\(UUID())")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: parent) }
        let dir = parent.appendingPathComponent("Shoot")
        try FileManager.default.moveItem(at: src, to: dir)
        let s = RustSessionStore(recents: recents(), storeBase: tempBase())
        let info = try await s.add(folder: dir)
        var p = try await s.photos(in: info)[0]
        let renamed = parent.appendingPathComponent("Shoot (renamed)")
        try FileManager.default.moveItem(at: dir, to: renamed)
        p.rating = 3
        try await s.save(photos: [p])
        let listed = try await s.photos(in: info)
        #expect(listed.first { $0.id == p.id }?.rating == 3)
        #expect(listed.allSatisfy { $0.url?.path.contains("Shoot (renamed)") == true })
    }

    @Test func cancellingTheFirstOpenLeavesNothingBehind() async throws {
        guard let dir = try sampleFolderCopy(limit: 3) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = RustSessionStore(recents: recents(), storeBase: tempBase())
        let t = Task { try await s.add(folder: dir) }
        t.cancel()
        await #expect(throws: CancellationError.self) { _ = try await t.value }
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent(".lt").path))
    }

    @Test func aReadOnlyFolderKeepsItsSessionInTheStoreBase() async throws {
        guard let dir = try sampleFolderCopy(limit: 2) else { return }
        let base = tempBase()
        defer {
            try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: dir.path)
            try? FileManager.default.removeItem(at: dir)
            try? FileManager.default.removeItem(at: base)
        }
        try FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: dir.path)
        let s = RustSessionStore(recents: recents(), storeBase: base)
        let info = try await s.add(folder: dir)
        var p = try await s.photos(in: info)[0]
        p.colorTag = .orange
        try await s.save(photos: [p])
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent(".lt").path))
        let store = RustSessionStore.fallbackStore(for: dir.resolvingSymlinksInPath(), base: base)
        #expect(FileManager.default.fileExists(atPath: store.appendingPathComponent(".lt/settings/session.json").path))
        let again = RustSessionStore(recents: recents(), storeBase: base)
        let listed = try await again.photos(in: try await again.add(folder: dir))
        #expect(listed.first { $0.id == p.id }?.colorTag == .orange, "orange survives (7 labels)")
    }

    @Test func autoApplyReachesNewlyRegisteredPhotosOnly() async throws {
        guard let dir = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        var e = EditState()
        e.exposure.ev = 0.75
        let preset = Preset(name: "Lift", groups: [.exposure], edit: e)
        let s = RustSessionStore(recents: recents(), storeBase: tempBase(), autoApply: { preset })
        let info = try await s.add(folder: dir)
        let photos = try await s.photos(in: info)
        #expect(photos.allSatisfy { $0.isEdited })
        let got = try await s.edit(for: photos[0].id)
        #expect(abs(got.exposure.ev - 0.75) < 1e-6)
    }

    @Test func cachesAreKeyedBySessionAndPhoto() async throws {
        guard let a = try sampleFolderCopy(limit: 1) else { return }
        let b = a.deletingLastPathComponent().appendingPathComponent("lt-dup-\(UUID())")
        defer {
            try? FileManager.default.removeItem(at: a)
            try? FileManager.default.removeItem(at: b)
        }
        let s = RustSessionStore(recents: recents(), storeBase: tempBase())
        let ia = try await s.add(folder: a)
        let pa = try await s.photos(in: ia)[0]
        // Duplicating the folder in Finder duplicates its session: same photo ids.
        try FileManager.default.copyItem(at: a, to: b)
        let ib = try await s.add(folder: b)
        let pb = try await s.photos(in: ib)[0]
        #expect(pa.id == pb.id)
        _ = try await s.photos(in: ia)
        var e = EditState()
        e.exposure.ev = 1
        try await s.save(edits: [pa.id: e])
        #expect(try await s.edit(for: pa.id) == e)
        _ = try await s.photos(in: ib)
        #expect(try await s.edit(for: pb.id) == EditState(), "B's photo, not A's cached edit")
        let eb = try await s.edits(in: ib), ea = try await s.edits(in: ia)
        #expect(eb.isEmpty && ea == [pa.id: e])
    }
}

/// A value shared with `@Sendable` progress callbacks.
private final class Locked<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ value: T) { self.value = value }
    func withLock<R>(_ body: (inout T) -> R) -> R {
        lock.lock()
        defer { lock.unlock() }
        return body(&value)
    }
}
