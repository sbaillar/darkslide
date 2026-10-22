import CoreGraphics
import Foundation
import ImageIO
import Metal
import simd
import Testing
import DarkslideFFI
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

private let srgbProfile = ProofProfile(name: "sRGB IEC61966-2.1", path: "/p/srgb.icc", kind: .display, model: .rgb)
private let adobeProfile = ProofProfile(name: "Adobe RGB (1998)", path: "/p/adobe.icc", kind: .display, model: .rgb)
private let paperProfile = ProofProfile(name: "Glossy Paper", path: "/p/paper.icc", kind: .printer, model: .cmyk)
private let monitorProfile = ProofProfile(name: "Studio Display", path: "/p/display.icc", kind: .display, model: .rgb)
private let systemProfiles = "/System/Library/ColorSync/Profiles"

private struct FixedCatalog: ProofProfileCatalog {
    var list = [paperProfile, adobeProfile, monitorProfile, srgbProfile]
    func profiles() -> [ProofProfile] { list }
}

/// Records each request; fails for paths containing "bad". With `gated`, each build
/// waits for `release()`.
private final class FakeBuilder: ProofTableBuilder, @unchecked Sendable {
    private let lock = NSLock()
    private var log: [(String, ProofIntent, Bool)] = []
    private let gate = DispatchSemaphore(value: 0)
    private var _gated = false
    var gated: Bool {
        get { lock.withLock { _gated } }
        set { lock.withLock { _gated = newValue } }
    }
    var requests: [(String, ProofIntent, Bool)] { lock.withLock { log } }

    func release() { gate.signal() }

    func table(profilePath: String, intent: ProofIntent, blackPointCompensation: Bool) throws -> ProofTable {
        lock.withLock { log.append((profilePath, intent, blackPointCompensation)) }
        if gated { gate.wait() }
        if profilePath.contains("bad") { throw DsError.InvalidArgument(message: "ICC profile: not a readable ICC profile") }
        return ProofTable(size: 2, rgba: [Float](repeating: 0, count: 32), profileName: profilePath,
                          hasIntents: profilePath.contains("paper"))
    }
}

@MainActor
private func makeProof(_ defaults: UserDefaults = testDefaults(), builder: FakeBuilder = FakeBuilder())
    -> (SoftProofModel, FakeBuilder) {
    (SoftProofModel(store: SoftProofStore(defaults: defaults), catalog: FixedCatalog(), builder: builder), builder)
}

/// An identity proof table (sRGB-encoded Rec.2020 in, linear Rec.2020 out): every texel
/// in gamut (alpha −1) except the pure-green corner (+1).
private func identityTable(size n: Int = 17) -> [Float] {
    var t: [Float] = []
    for b in 0..<n { for g in 0..<n { for r in 0..<n {
        let e = [r, g, b].map { Float($0) / Float(n - 1) }
        t += e.map { $0 <= 0.04045 ? $0 / 12.92 : powf(($0 + 0.055) / 1.055, 2.4) }
        t.append(r == 0 && g == n - 1 && b == 0 ? 1 : -1)
    } } }
    return t
}

@MainActor
@Suite struct SoftProofModelTests {
    @Test func togglePicksSRGBBuildsTheTableAndShowsIt() async throws {
        let (m, b) = makeProof()
        var changes = 0
        m.onDisplayChange = { changes += 1 }
        #expect(m.display == nil && m.statusText == nil)
        m.toggle()
        #expect(m.enabled)
        #expect(await waitUntil { m.table != nil })
        #expect(m.settings.profile == srgbProfile, "no profile yet: sRGB, from the catalog scanned off-main")
        #expect(b.requests.count == 1)
        let request = try #require(b.requests.first) // a timed-out wait fails here, not traps
        #expect(request.0 == "/p/srgb.icc")
        #expect(request.1 == .perceptual && request.2, "defaults: perceptual with BPC")
        let d = try #require(m.display)
        #expect(d.tableID == m.table?.id && !d.gamutWarning)
        #expect(m.statusText == "Proof: /p/srgb.icc", "the table's profile (the fake names it by path)")
        #expect(!m.intentsApply, "a matrix profile: the intent picker hides")
        #expect(changes >= 2, "on, then the table landed")
        m.toggle()
        #expect(!m.enabled && m.table == nil && m.display == nil)
        m.setEnabled(false)
        #expect(!m.enabled, "no-op")
        m.setProfile(paperProfile)
        m.setEnabled(true)
        #expect(await waitUntil { m.table?.profileName == "/p/paper.icc" })
        #expect(m.intentsApply, "a LUT profile has intents")
        #expect(b.requests.count == 2, "the catalog isn't rescanned and nothing extra is built")
    }

    @Test func settingChangesRebuildAndPersistInTheInjectedSuite() async throws {
        let defaults = testDefaults()
        let (m, b) = makeProof(defaults)
        m.setProfile(adobeProfile)
        m.setIntent(.relativeColorimetric)
        m.setBlackPointCompensation(false)
        #expect(b.requests.isEmpty, "off: nothing is built")
        m.setEnabled(true)
        #expect(m.statusText == "Proof: preparing Adobe RGB (1998)…", "no table yet")
        #expect(await waitUntil { m.table != nil })
        let request = try #require(b.requests.last)
        #expect(request == ("/p/adobe.icc", .relativeColorimetric, false))
        let first = try #require(m.table?.id)
        m.setIntent(.perceptual)
        #expect(await waitUntil { m.table.map { $0.id != first } ?? false })
        #expect(b.requests.count == 2)
        m.setIntent(.perceptual)
        #expect(b.requests.count == 2, "an unchanged setting rebuilds nothing")
        m.setGamutWarning(true)
        m.setWarningColor(.magenta)
        #expect(m.display?.gamutWarning == true && m.display?.warningColor == SIMD3(1, 0, 1))
        #expect(b.requests.count == 2, "the warning is drawn, not built")
        // Everything but on/off comes back from the suite the store was given.
        let (again, _) = makeProof(defaults)
        #expect(again.settings == m.settings && !again.enabled)
        #expect(defaults.data(forKey: SoftProofStore.key) != nil)
        // Unreadable saved data falls back to the defaults.
        defaults.set(Data("junk".utf8), forKey: SoftProofStore.key)
        #expect(SoftProofStore(defaults: defaults).load() == SoftProofSettings())
    }

    /// One build at a time: changes during a build are coalesced into one more build for
    /// the latest settings, and the old table stays on screen meanwhile (no unproofed
    /// flash).
    @Test func supersededBuildsAreSkippedAndTheOldTableStays() async throws {
        let (m, b) = makeProof()
        m.setProfile(adobeProfile)
        m.setEnabled(true)
        #expect(await waitUntil { m.table != nil })
        let old = try #require(m.display)
        b.gated = true
        m.setIntent(.relativeColorimetric)
        #expect(await waitUntil { b.requests.count == 2 })
        m.setBlackPointCompensation(false)
        m.setProfile(srgbProfile)
        m.setIntent(.perceptual)
        #expect(m.display == old, "still the previous proof while building")
        #expect(m.statusText == "Proof: /p/adobe.icc", "the HUD names the profile actually drawn")
        b.gated = false
        b.release()
        #expect(await waitUntil { m.table?.profileName == "/p/srgb.icc" })
        #expect(b.requests.count == 3, "the two intermediate settings were never built: \(b.requests)")
        #expect(b.requests.last! == ("/p/srgb.icc", .perceptual, false))
        // Turned off mid-build: the late table isn't shown.
        b.gated = true
        m.setIntent(.relativeColorimetric)
        #expect(await waitUntil { b.requests.count == 4 })
        m.setEnabled(false)
        b.gated = false
        b.release()
        await m.buildTask?.value
        #expect(m.table == nil && m.display == nil)
    }

    @Test func failuresAreShown() async throws {
        let (m, _) = makeProof()
        m.setProfile(ProofProfile(name: "Broken", path: "/p/bad.icc", kind: .printer, model: .rgb))
        m.setEnabled(true)
        #expect(await waitUntil { m.error != nil })
        #expect(m.error == "not a readable ICC profile")
        #expect(m.statusText == "Proof failed: not a readable ICC profile" && m.display == nil)
        #expect(m.table == nil, "a failed build leaves no stale proof")
        m.setProfile(srgbProfile)
        #expect(await waitUntil { m.table != nil })
        #expect(m.error == nil)
        #expect(SoftProofModel.plain(CancellationError()).contains("CancellationError"))
    }

    @Test func profilesAreGroupedAndOnlyRGBGoesToExport() async {
        let (m, _) = makeProof()
        await m.loadProfiles()
        #expect(m.profiles.map(\.name) == ["Adobe RGB (1998)", "sRGB IEC61966-2.1", "Studio Display", "Glossy Paper"])
        #expect(m.profileGroups.map(\.kind) == [.display, .printer])
        #expect(m.profileGroups.map(\.kind.title) == ["Screens and color spaces", "Printers and papers"])
        #expect(m.recipeProfile == nil, "no profile chosen")
        m.setProfile(paperProfile)
        #expect(m.recipeProfile == nil, "CMYK can't be an export profile")
        m.setProfile(adobeProfile)
        m.setBlackPointCompensation(false)
        #expect(m.recipeProfile == RecipeOutputProfile(name: "Adobe RGB (1998)", path: "/p/adobe.icc",
                                                       intent: .perceptual, blackPointCompensation: false))
        let empty = SoftProofModel(store: SoftProofStore(defaults: testDefaults()),
                                   catalog: FixedCatalog(list: []), builder: FakeBuilder())
        empty.setEnabled(true)
        #expect(await waitUntil { empty.error != nil })
        #expect(empty.error == "No profile chosen" && empty.settings.profile == nil)
        #expect(ProofIntent.allCases.map(\.title) == ["Perceptual", "Relative colorimetric"])
        #expect(GamutWarningColor.allCases.map(\.title).first == "Gray")
        #expect(GamutWarningColor.allCases.allSatisfy { simd_reduce_max($0.linearP3) <= 1 })
    }

    @Test func shiftSTogglesTheProofWithAToastAndAWideRender() async {
        let m = makeModel()
        m.softProof.builder = FakeBuilder()
        m.softProof.catalog = FixedCatalog()
        #expect(KeyboardRouter.command(for: KeyInput("S", shift: true)) == .toggleSoftProof)
        #expect(KeyboardRouter.command(for: KeyInput("s")) == .toggleStack, "plain S still stacks")
        #expect(KeyboardRouter.cheatSheet().flatMap(\.rows).contains { $0.keys == "⇧S" })
        let before = m.viewer.displayVersion
        m.perform(.toggleSoftProof)
        #expect(m.toast?.message == "Soft proof on")
        #expect(m.viewer.proofWide, "frames are rendered in Rec.2020 while proofing")
        #expect(await waitUntil { m.viewer.proofTable != nil })
        #expect(m.viewer.displayVersion > before)
        #expect(m.viewer.drawPlan(loupeAt: nil).quads.allSatisfy { $0.proof == m.softProof.display })
        m.perform(.toggleSoftProof)
        #expect(m.toast?.message == "Soft proof off" && m.viewer.proofTable == nil && !m.viewer.proofWide)
    }

    @Test func proofingRendersTheWorkingGamutWhereTheRendererCan() {
        let float = RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true, rendersGeometry: true)
        let eight = RendererCapabilities(regions: true, floatFrames: false, rendersEdits: true, rendersGeometry: true)
        #expect(ViewerModel.renderFormat(proof: true, edrHeadroom: 1, wideGamut: false, capabilities: float) == .rgba16fLinearRec2020)
        #expect(ViewerModel.renderFormat(proof: true, edrHeadroom: 2, wideGamut: true, capabilities: eight) == .rgba8DisplayP3)
        #expect(ViewerModel.renderFormat(proof: false, edrHeadroom: 2, wideGamut: false, capabilities: float) == .rgba16fLinearP3)
        let f = FramePixelFormat.rgba16fLinearRec2020
        #expect(f.isFloat && !f.isEightBit && !f.isP3 && f.bytesPerPixel == 8)
        #expect(MetalViewerRenderer.textureFormat(for: f) == .rgba16Float)
    }
}

@Suite struct ProofCatalogTests {
    @Test func colorSyncEntriesMapToProfiles() {
        let p = ColorSyncProfileCatalog.profile(path: "/x/Paper.icc", classSig: "prtr", spaceSig: "CMYK", description: nil)
        #expect(p == ProofProfile(name: "Paper", path: "/x/Paper.icc", kind: .printer, model: .cmyk))
        #expect(ColorSyncProfileCatalog.profile(path: "/x/a.icc", classSig: "mntr", spaceSig: "RGB ", description: "A")?.kind
            == .display)
        let g = ColorSyncProfileCatalog.profile(path: "/x/g.icc", classSig: "spac", spaceSig: "GRAY", description: "")
        #expect(g?.name == "g" && g?.kind == .display && g?.model == .gray)
        #expect(ColorSyncProfileCatalog.profile(path: "/x/s.icc", classSig: "scnr", spaceSig: "RGB ", description: "S") == nil,
                "input profiles can't be proofed against")
        #expect(ColorSyncProfileCatalog.profile(path: "/x/l.icc", classSig: "spac", spaceSig: "Lab ", description: "L") == nil)
        #expect(ColorSyncProfileCatalog.profile(path: nil, classSig: "spac", spaceSig: "RGB", description: "N") == nil)
        #expect(ColorSyncProfileCatalog.profile([:]) == nil)
    }

    /// The installed system profiles (real ColorSync) and a real table from Little CMS.
    @Test func liveCatalogAndRustBuilder() throws {
        let all = ColorSyncProfileCatalog().profiles()
        let srgb = try #require(all.first { $0.model == .rgb && $0.name.contains("sRGB") }, "\(all.map(\.name))")
        #expect(Set(all.map(\.path)).count == all.count, "no duplicates")
        let t = try RustProofTableBuilder().table(profilePath: srgb.path, intent: .relativeColorimetric,
                                                  blackPointCompensation: false)
        #expect(t.size == 33 && t.rgba.count == 33 * 33 * 33 * 4 && !t.hasIntents)
        // White is white and in gamut; the pure Rec.2020 green corner is out of sRGB.
        let white = Array(t.rgba.suffix(4))
        #expect(white.prefix(3).allSatisfy { abs($0 - 1) < 0.01 } && white[3] < 0, "\(white)")
        #expect(t.rgba[(32 * 33) * 4 + 3] > 0)
        #expect(throws: DsError.self) {
            try RustProofTableBuilder().table(profilePath: "/nonexistent.icc", intent: .perceptual, blackPointCompensation: true)
        }
    }
}

@MainActor
@Suite struct SoftProofDrawTests {
    @Test func planCarriesTheProofWithItsMatrices() {
        var s = ViewerState()
        s.viewSize = CGSize(width: 400, height: 300)
        let l = ViewerLayout.make(state: s, main: CGSize(width: 600, height: 400), pinned: nil)
        var input = ViewerDrawPlan.Input(layout: l, viewSize: s.viewSize, backingScale: 2, main: solidFrame([9, 9, 9]))
        #expect(ViewerDrawPlan.make(input).quads[0].proof == nil)
        let proof = ProofDisplay(tableID: 7, gamutWarning: true, warningColor: SIMD3(1, 0, 1))
        input.proof = proof
        let q = ViewerDrawPlan.make(input).quads[0]
        #expect(q.proof == proof)
        #expect(q.proofIn == ViewerColor.toRec2020(.rgba8Srgb) && q.proofOut == ViewerColor.rec2020ToSrgb,
                "sRGB frame on an sRGB surface")
        #expect(q.gamutColor == ViewerColor.p3ToSrgb * SIMD3(1, 0, 1))
        #expect(q.neutralTolerance == 0)
        input.proof?.neutralTolerance = 0.02
        #expect(ViewerDrawPlan.make(input).quads[0].neutralTolerance == 0.02, "gray profiles: a chroma test")
        input.main = floatFrame(0.5)
        let e = ViewerDrawPlan.make(input).quads[0]
        #expect(e.proofIn == ViewerColor.toRec2020(.rgba16fLinearP3) && e.proofOut == ViewerColor.rec2020ToP3, "P3 frame on EDR")
        #expect(e.gamutColor == SIMD3(1, 0, 1))
        // The matrices round-trip.
        let rt = ViewerColor.rec2020ToP3 * ViewerColor.toRec2020(.rgba16fLinearP3)
        #expect(abs(rt[0][0] - 1) < 1e-4 && abs(rt[1][0]) < 1e-4)
        #expect(ViewerColor.toRec2020(.rgba16fLinearRec2020) == matrix_identity_float3x3)
        #expect(ViewerColor.matrix(source: .rgba16fLinearRec2020, p3Target: false) == ViewerColor.rec2020ToSrgb)
    }

    @Test func shaderProofsAndPaintsTheGamutWarning() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let lut = try #require(MetalViewerRenderer.makeProofTexture(device: m.device, size: 17, rgba: identityTable()))
        #expect(MetalViewerRenderer.makeProofTexture(device: m.device, size: 17, rgba: [1, 2]) == nil)
        // An identity proof leaves an sRGB colour as it was.
        var pane = MetalViewerRenderer.Pane(after: try #require(m.makeTexture(from: solidFrame([200, 30, 30]))),
                                            rect: SIMD4(-1, -1, 1, 1))
        pane.proofLUT = lut
        pane.proofIn = ViewerColor.toRec2020(.rgba8Srgb)
        pane.proofOut = ViewerColor.fromRec2020(p3Target: false)
        let out = try #require(m.renderOffscreen([pane], width: 4, height: 4, clear: MTLClearColor()))
        #expect(zip(out.prefix(3), [200, 30, 30]).allSatisfy { abs(Int($0) - $1) <= 3 }, "\(Array(out.prefix(3)))")
        // A pure Rec.2020 green (a working-gamut frame): the warning colour when asked.
        var green = [Float16](repeating: 0, count: 4 * 4 * 4)
        for i in stride(from: 0, to: green.count, by: 4) { green[i + 1] = 1; green[i + 3] = 1 }
        let gf = RenderedFrame(width: 4, height: 4, format: .rgba16fLinearRec2020, bytesPerRow: 32,
                               backing: .data(green.withUnsafeBytes { Data($0) }))
        var g = MetalViewerRenderer.Pane(after: try #require(m.makeTexture(from: gf)), rect: SIMD4(-1, -1, 1, 1))
        g.proofLUT = lut
        g.proofOut = ViewerColor.fromRec2020(p3Target: false)
        g.gamutWarning = true
        g.gamutColor = SIMD3(0.5, 0.5, 0.5)
        let warned = try #require(m.renderOffscreen([g], width: 4, height: 4, clear: MTLClearColor()))
        #expect(warned.prefix(3).allSatisfy { abs(Int($0) - 188) <= 2 }, "linear 0.5 gray: \(Array(warned.prefix(3)))")
        g.gamutWarning = false
        let plain = try #require(m.renderOffscreen([g], width: 4, height: 4, clear: MTLClearColor()))
        #expect(plain[1] == 255 && plain[0] < 10, "the green itself (clipped to sRGB): \(Array(plain.prefix(3)))")
    }

    /// Gray proof profiles: the shader tests chroma, so neutrals (between grid levels
    /// too) are never painted and tints are.
    @Test func grayProfilesUseTheChromaTest() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        // A table whose alpha says "out" everywhere: only the chroma test can pass neutrals.
        var t = identityTable()
        for i in stride(from: 3, to: t.count, by: 4) { t[i] = 1 }
        let lut = try #require(MetalViewerRenderer.makeProofTexture(device: m.device, size: 17, rgba: t))
        func draw(_ c: SIMD3<Float16>) throws -> [UInt8] {
            var px = [Float16](repeating: 1, count: 4 * 4 * 4)
            for i in stride(from: 0, to: px.count, by: 4) { px[i] = c.x; px[i + 1] = c.y; px[i + 2] = c.z }
            let f = RenderedFrame(width: 4, height: 4, format: .rgba16fLinearRec2020, bytesPerRow: 32,
                                  backing: .data(px.withUnsafeBytes { Data($0) }))
            var p = MetalViewerRenderer.Pane(after: try #require(m.makeTexture(from: f)), rect: SIMD4(-1, -1, 1, 1))
            p.proofLUT = lut
            p.proofOut = ViewerColor.fromRec2020(p3Target: false)
            p.gamutWarning = true
            p.gamutColor = SIMD3(1, 0, 1)
            p.neutralTolerance = 0.02
            return try #require(m.renderOffscreen([p], width: 4, height: 4, clear: MTLClearColor()))
        }
        for v in [0.013, 0.21, 0.37, 0.6] as [Float16] {
            let o = try draw(SIMD3(v, v, v))
            #expect(abs(Int(o[0]) - Int(o[1])) <= 1 && abs(Int(o[1]) - Int(o[2])) <= 1,
                    "neutral \(v) shown as itself: \(Array(o.prefix(3)))")
        }
        let tinted = try draw(SIMD3(0.3, 0.2, 0.3))
        #expect(tinted[0] > 240 && tinted[1] < 20, "a tint is out of a gray gamut: \(Array(tinted.prefix(3)))")
    }

    /// While proofing, frames are in the working gamut: the clipping warning judges the
    /// colour in the surface's primaries, so a colour that clips to 0 there is marked.
    @Test func clippingIsJudgedInTheSurfacesPrimaries() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        var px = [Float16](repeating: 1, count: 4 * 4 * 4)
        for i in stride(from: 0, to: px.count, by: 4) { px[i] = 0.2; px[i + 1] = 0.9; px[i + 2] = 0.2 }
        let f = RenderedFrame(width: 4, height: 4, format: .rgba16fLinearRec2020, bytesPerRow: 32,
                              backing: .data(px.withUnsafeBytes { Data($0) }))
        var p = MetalViewerRenderer.Pane(after: try #require(m.makeTexture(from: f)), rect: SIMD4(-1, -1, 1, 1))
        p.colorMatrix = ViewerColor.matrix(source: .rgba16fLinearRec2020, p3Target: false)
        p.clippingWarning = true
        let o = try #require(m.renderOffscreen([p], width: 4, height: 4, clear: MTLClearColor()))
        // The shadow-clip tint (linear 0.05, 0.25, 1.0 → sRGB 63, 137, 255).
        #expect(o[2] == 255 && abs(Int(o[0]) - 63) <= 2 && abs(Int(o[1]) - 137) <= 2,
                "red clips to 0 in sRGB: shadow clip blue, \(Array(o.prefix(3)))")
        // The loupe's flag likewise reads display (P3) values: Rec.2020 (0.9, 0.9, 0.9) is
        // under P3 white; a green under 1 in Rec.2020 but over 1 in P3 is flagged.
        func clipped(_ c: SIMD3<Float16>) -> Bool {
            var q = [Float16](repeating: 1, count: 4 * 4 * 4)
            for i in stride(from: 0, to: q.count, by: 4) { q[i] = c.x; q[i + 1] = c.y; q[i + 2] = c.z }
            let fr = RenderedFrame(width: 4, height: 4, format: .rgba16fLinearRec2020, bytesPerRow: 32,
                                   backing: .data(q.withUnsafeBytes { Data($0) }))
            let r = PickerLoupe.Region(frame: fr, rect: CGRect(x: 0, y: 0, width: 1, height: 1),
                                       space: CGSize(width: 4, height: 4), photo: PhotoID(1), edit: EditState())
            return PickerLoupe.measure(r, at: LoupePixel(x: 1, y: 1), sample: 1)?.clipped ?? false
        }
        #expect(!clipped(SIMD3(0.9, 0.9, 0.9)))
        #expect(clipped(SIMD3(0, 0.95, 0)), "its P3 green channel passes 1 (1.02)")
    }

    /// Cost of the proof in the viewer's draw: a 5K fit frame (2832×1888) into a 5K
    /// surface, GPU time with and without the proof and gamut warning.
    @Test func proofCostsAFractionOfAMillisecond() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let (w, h) = (2832, 1888)
        var px = [UInt8](repeating: 0, count: w * h * 4)
        for i in stride(from: 0, to: px.count, by: 4) { px[i] = UInt8(i / 4 % w * 255 / w); px[i + 1] = 90; px[i + 2] = 160; px[i + 3] = 255 }
        let tex = try #require(m.makeTexture(from: RenderedFrame(width: w, height: h, rgba: Data(px))))
        let lut = try #require(MetalViewerRenderer.makeProofTexture(device: m.device, size: 33,
                                                                    rgba: identityTable(size: 33)))
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .rgba8Unorm_srgb, width: w, height: h, mipmapped: false)
        desc.usage = [.renderTarget]
        desc.storageMode = .private
        let target = try #require(m.device.makeTexture(descriptor: desc))
        func gpuMs(_ pane: MetalViewerRenderer.Pane) -> Double {
            var times: [Double] = []
            for _ in 0..<30 {
                let cb = m.makeCommandBuffer()!
                let pass = MTLRenderPassDescriptor()
                pass.colorAttachments[0].texture = target
                pass.colorAttachments[0].loadAction = .clear
                pass.colorAttachments[0].storeAction = .store
                m.encode([pane], pass: pass, commandBuffer: cb, viewPixelWidth: w, targetFormat: .rgba8Unorm_srgb)
                cb.commit()
                cb.waitUntilCompleted()
                times.append((cb.gpuEndTime - cb.gpuStartTime) * 1000)
            }
            return times.sorted()[times.count / 2]
        }
        let plain = MetalViewerRenderer.Pane(after: tex, rect: SIMD4(-1, -1, 1, 1))
        var proofed = plain
        proofed.proofLUT = lut
        proofed.proofIn = ViewerColor.toRec2020(.rgba8Srgb)
        proofed.proofOut = ViewerColor.fromRec2020(p3Target: false)
        proofed.gamutWarning = true
        let (off, on) = (gpuMs(plain), gpuMs(proofed))
        print("soft proof draw 2832x1888: off \(String(format: "%.3f", off)) ms, on \(String(format: "%.3f", on)) ms (median GPU)")
        #expect(on < 4, "proofed draw \(on) ms")
    }
}

@MainActor
@Suite struct ProofExportTests {
    @Test func recipesCarryTheProofProfileToTheFFI() throws {
        let p = RecipeOutputProfile(name: "Adobe RGB (1998)", path: "/p/adobe.icc", intent: .relativeColorimetric,
                                    blackPointCompensation: true)
        var r = ExportRecipe(name: "Print", format: .tiff16)
        #expect(r.summary.hasSuffix("sRGB"))
        r.outputProfile = p
        #expect(r.summary.contains("Adobe RGB (1998)") && !r.summary.contains("sRGB"))
        let spec = RustExportService.spec(r, defaultFolder: URL(fileURLWithPath: "/tmp"))
        #expect(spec.outputProfile == ExportOutputProfile(path: "/p/adobe.icc", intent: .relativeColorimetric,
                                                          blackPointCompensation: true))
        #expect(RustExportService.spec(ExportRecipe(name: "x"), defaultFolder: URL(fileURLWithPath: "/tmp")).outputProfile == nil)
        // Saved and restored; recipes saved before the field existed have none.
        let back = try JSONDecoder().decode(ExportRecipe.self, from: JSONEncoder().encode(r))
        #expect(back.outputProfile == p)
        var json = try #require(JSONSerialization.jsonObject(with: JSONEncoder().encode(ExportRecipe(name: "old"))) as? [String: Any])
        json["outputProfile"] = nil
        let old = try JSONDecoder().decode(ExportRecipe.self, from: JSONSerialization.data(withJSONObject: json))
        #expect(old.outputProfile == nil)
    }

    /// Validation reads the profile file through Rust, and refuses a linear-light
    /// profile for an 8-bit format.
    @Test func validationReadsTheProfile() throws {
        let srgb = try #require(ColorSyncProfileCatalog().profiles().first { $0.model == .rgb && $0.name.contains("sRGB") })
        var r = ExportRecipe(name: "Proofed", format: .png8)
        r.outputProfile = RecipeOutputProfile(name: srgb.name, path: srgb.path, intent: .perceptual,
                                              blackPointCompensation: true)
        let service = RustExportService()
        #expect(service.validate([r], defaultFolder: URL(fileURLWithPath: "/tmp")) == nil)
        r.outputProfile?.path = "/nonexistent/x.icc"
        let msg = try #require(service.validate([r], defaultFolder: URL(fileURLWithPath: "/tmp")))
        #expect(msg.hasPrefix("Proofed:"), "\(msg)")
        let aces = "\(systemProfiles)/ACESCG Linear.icc"
        guard FileManager.default.fileExists(atPath: aces) else { return }
        r.outputProfile?.path = aces
        let linear = try #require(service.validate([r], defaultFolder: URL(fileURLWithPath: "/tmp")))
        #expect(linear.contains("linear-light"), "\(linear)")
        r.format = .tiff16
        #expect(service.validate([r], defaultFolder: URL(fileURLWithPath: "/tmp")) == nil)
    }

    /// HEIC and ImageIO JPEG embed the output profile, and refuse one ImageIO can't
    /// make a colour space from instead of tagging the pixels sRGB.
    @Test func imageIOEmbedsTheProfileOrFails() throws {
        let adobe = "\(systemProfiles)/AdobeRGB1998.icc"
        guard let icc = FileManager.default.contents(atPath: adobe) else { return }
        let enc = ImageIOEncoder()
        let px = Data(repeating: 0x80, count: 8 * 8 * 6)
        for format in [EncodedImageFormat.heic, .jpeg] {
            let data = try enc.encode(format: format, width: 8, height: 8, rgb16: px, quality: 90, icc: icc, exif: nil,
                                      keywords: [], metadata: RustSessionStore.record(IPTCMetadata()))
            let src = try #require(CGImageSourceCreateWithData(data as CFData, nil))
            let image = try #require(CGImageSourceCreateImageAtIndex(src, 0, nil))
            #expect(image.colorSpace?.name as String? == CGColorSpace.adobeRGB1998 as String, "\(format)")
            #expect(throws: DsError.self) {
                try enc.encode(format: format, width: 8, height: 8, rgb16: px, quality: 90, icc: Data("junk".utf8),
                               exif: nil, keywords: [], metadata: RustSessionStore.record(IPTCMetadata()))
            }
        }
    }
}
