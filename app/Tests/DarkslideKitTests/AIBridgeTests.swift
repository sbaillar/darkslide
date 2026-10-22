import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideKit

private func result(_ id: Int64, _ score: Float, _ s: PickSuggestionKind, group: UInt32 = 0,
                    reasons: [ReasonRecord] = []) -> CullResultRecord {
    CullResultRecord(id: id, score: score, sharpness: score, exposure: 1, noise: nil, reasons: reasons, group: group,
                     suggestion: s)
}

private let hardReason = ReasonRecord(kind: "out_of_focus", text: "out of focus", hard: true, negative: true)

@Suite struct DSAIPolicyTests {
    @Test func burstBestIsPickedRejectsRejected() {
        let best = DSAI.suggestion(for: result(1, 0.5, .best), groupSize: 3)
        #expect(best.pick == .picked && best.reasons.first == "Best of 3 similar frames")
        #expect(DSAI.suggestion(for: result(2, 0.44, .best), groupSize: 3).pick == .none, "poor best stays undecided")
        #expect(DSAI.suggestion(for: result(3, 0.9, .reject), groupSize: 3).pick == .rejected)
        #expect(DSAI.suggestion(for: result(4, 0.6, .keep), groupSize: 3).pick == .none)
    }

    @Test func loneFramesNeedAHighScoreAndHardFailuresReject() {
        #expect(DSAI.suggestion(for: result(1, 0.69, .best), groupSize: 1).pick == .none)
        #expect(DSAI.suggestion(for: result(1, 0.7, .best), groupSize: 1).pick == .picked)
        #expect(DSAI.suggestion(for: result(1, 0.29, .best), groupSize: 1).pick == .rejected)
        let hard = DSAI.suggestion(for: result(1, 0.9, .best, reasons: [hardReason]), groupSize: 1)
        #expect(hard.pick == .rejected && hard.reasons == ["Out of focus"])
    }

    @Test func reportMapsOntoPhotosInOrder() {
        let ps = MockSessionStore.makePhotos(count: 3, firstID: 10, start: Date())
        let report = CullReport(results: [result(10, 0.8, .best), result(11, 0.2, .reject)],
                                groups: [GroupRecord(kind: .burst, memberIds: [10, 11])])
        let out = DSAI.suggestions(report, photos: ps)
        #expect(out.map(\.photoID) == ps.map(\.id))
        #expect(out[0].pick == .picked && out[1].pick == .rejected)
        #expect(out[2].reasons == ["Could not analyze"] && out[2].pick == .none)
    }
}

@Suite struct DSAIConversionTests {
    @Test func visionSignalsFlipToTopLeftAndCarryAF() {
        let face = FaceInfo(boundingBox: CGRect(x: 0.2, y: 0.1, width: 0.4, height: 0.3), captureQuality: 0.7,
                            leftEyeAspect: 0.3, rightEyeAspect: 0.3)
        let a = DarkslideAI.ImageAnalysis(
            statistics: ImageStatistics(width: 1, height: 1, meanLinearRGB: .zero, meanLuma: 0.5, highlightClip: 0, shadowClip: 0,
                                        laplacianVariance: 0),
            faces: [face], aesthetics: AestheticsScore(overall: -0.5, isUtility: true), featurePrint: nil, labels: [],
            horizonDegrees: nil, salientRegions: [CGRect(x: 0, y: 0, width: 0.5, height: 0.25)])
        let v = DSAI.signals(a, afPoint: CGPoint(x: 0.4, y: 0.6))
        let f = v.faces[0]
        #expect(f.bounds == NormRect(x: 0.2, y: 0.6, w: 0.4, h: 0.3), "Vision y is bottom-up")
        #expect(abs(f.area! - 0.12) < 1e-6 && f.eyesOpen == 1 && f.captureQuality == 0.7)
        #expect(v.aesthetics == -0.5 && v.isUtility == true)
        #expect(v.afPoint == NormPoint(x: 0.4, y: 0.6))
        #expect(v.saliency == [NormRect(x: 0, y: 0.75, w: 0.5, h: 0.25)])
        let empty = DSAI.signals(nil, afPoint: nil)
        #expect(empty.faces.isEmpty && empty.aesthetics == nil && empty.afPoint == nil)
    }

    @Test func seqPrefersShotNumberAndRgbaIsPacked() {
        var m = MockSessionStore.makePhotos(count: 1, firstID: 1, start: Date())[0].meta
        #expect(DSAI.seq(m) == nil)
        m.fileSequence = 7062
        #expect(DSAI.seq(m) == 7062)
        m.shotNumber = 41
        #expect(DSAI.seq(m) == 41)
        m.shotNumber = -1
        #expect(DSAI.seq(m) == nil)
        #expect(DSAI.captureMs(Date(timeIntervalSince1970: 1.2345)) == 1235)
        let img = MockImageProvider.draw(seed: 3, width: 30, height: 20)!
        let px = DSAI.rgba(img)!
        #expect(px.width == 30 && px.height == 20 && px.bytes.count == 30 * 20 * 4)
    }

    @Test func intentRecordMapsEveryField() {
        let r = EditIntentRecord(warmth: 25, temperatureK: 4000, temperatureShiftK: 200, tint: 5, exposureEv: 0.5, contrast: 10,
                                 saturation: 5, shadows: 20, highlights: 15,
                                 bandEdits: [BandEditRecord(band: "blue", saturation: 20, lightness: -10),
                                             BandEditRecord(band: "ultraviolet", saturation: 1, lightness: 1)],
                                 cropWidth: 4, cropHeight: 5, straightenAuto: true, straightenDegrees: 2,
                                 blackAndWhite: true, auto: true, autoWhiteBalance: true, unrecognized: ["zorble"])
        let i = DSAI.intent(from: r)
        #expect(i.warmth == 25 && i.temperatureTarget == 4000 && i.temperatureShift == 200 && i.tintShift == 5)
        #expect(i.exposureShift == 0.5 && i.contrastShift == 10 && i.shadowsShift == 20 && i.highlightsShift == 15)
        #expect(i.saturationShift == -100, "black and white")
        #expect(i.bandShifts == [BandShift(band: 5, saturation: 20, lightness: -10)])
        #expect(i.cropAspect == "4:5" && i.straighten && i.straightenDegrees == 2)
        #expect(i.unrecognized.count == 3 && i.unrecognized[0] == "zorble")
    }

    @Test func rulesServiceUsesDSAIParser() async throws {
        let i = try await RulesEditService().intent(for: "lift the shadows, bluer sky, level the horizon")
        #expect(i.shadowsShift > 0 && i.straighten)
        #expect(i.bandShifts.contains { $0.band == 5 })
        let none = try await RulesEditService().intent(for: "zorble the flurbs")
        #expect(none.isEmpty && !none.unrecognized.isEmpty)
    }
}

@Suite struct AIEditMergeTests {
    @Test func keepsUserChangesMadeDuringTheRun() {
        var base = EditState()
        base.exposure.contrast = 10
        var result = base
        result.whiteBalance.asShot = false
        result.whiteBalance.temperature = 6100
        result.exposure.ev = 0.4
        result.hdr.highlights = 20
        result.colorEditor.bands[2].saturation = 15
        result.crop.angle = -1.5
        var current = base
        current.exposure.contrast = 40      // user dragged contrast while AI ran
        current.exposure.ev = 1.0           // …and exposure too: AI wins on fields it changed
        current.vignette.amountEV = -0.3
        let out = AIEditMerge.apply(result: result, base: base, current: current)
        #expect(out.exposure.contrast == 40 && out.vignette.amountEV == -0.3)
        #expect(out.exposure.ev == 0.4 && out.whiteBalance.temperature == 6100 && !out.whiteBalance.asShot)
        #expect(out.hdr.highlights == 20 && out.colorEditor.bands[2].saturation == 15 && out.crop.angle == -1.5)
        #expect(AIEditMerge.apply(result: base, base: base, current: current) == current, "no-op result changes nothing")
    }

    @Test func wholeGroupsMergeWhenChanged() {
        let base = EditState()
        var result = base
        result.curve.points[.rgb] = [CurvePoint(0.25, 0.2)]
        result.layers = [LayerEdit(id: 0, name: "AI")]
        result.detail.clarity = 10
        var current = base
        current.noise.luminance = 60
        let out = AIEditMerge.apply(result: result, base: base, current: current)
        #expect(out.curve == result.curve && out.layers.count == 1 && out.detail.clarity == 10 && out.noise.luminance == 60)
    }

    /// CR-3: whole groups that only went through Rust's f32 storage and back (a hand-dragged
    /// crop, a layer's values) aren't the AI's change: the user's edit and its rows stay.
    @Test func roundOffInGroupsIsNotAChange() {
        func f32(_ v: Double) -> Double { Double(Float(v)) }
        var base = EditState()
        base.crop.rect = CGRect(x: 0.123456789, y: 0.0987654321, width: 0.7654321, height: 0.6543219)
        var layer = LayerEdit(id: 1, name: "Dodge", opacity: 0.876543219, exposureEV: 0.333333333)
        base.layers = [layer]
        base.curve.points[.rgb] = [CurvePoint(0.251234567, 0.2012345678)]
        base.vignette.amountEV = -0.4123456789
        var result = base // as Rust hands it back: f32 round-off everywhere, exposure changed
        result.crop.rect = CGRect(x: f32(0.123456789), y: f32(0.0987654321), width: f32(0.7654321), height: f32(0.6543219))
        layer.opacity = f32(layer.opacity)
        layer.exposureEV = f32(layer.exposureEV)
        result.layers = [layer]
        result.curve.points[.rgb] = [CurvePoint(f32(0.251234567), f32(0.2012345678))]
        result.vignette.amountEV = f32(-0.4123456789)
        result.exposure.ev = 0.6
        #expect(result.crop.rect != base.crop.rect && result.layers != base.layers)
        var current = base
        current.crop.rect.origin.x = 0.2 // dragged while the AI ran
        let out = AIEditMerge.apply(result: result, base: base, current: current)
        #expect(out.crop.rect == current.crop.rect && out.layers == base.layers)
        #expect(out.curve == base.curve && out.vignette == base.vignette && out.exposure.ev == 0.6)
        #expect(AIFieldMap.changed(current, out).count == 1, "only the exposure row reads as changed by AI")
        // A real change in a group is still the AI's.
        result.crop.rect.origin.x = 0.3
        result.layers[0].exposureEV = 0.5
        let moved = AIEditMerge.apply(result: result, base: base, current: current)
        #expect(moved.crop.rect == result.crop.rect && moved.layers == result.layers)
    }
}

@MainActor
@Suite struct StackGroupingTests {
    func burstPhotos() -> [Photo] {
        // Whole-second stamps (as A9 ARWs), ordered only by shot number.
        let t = Date(timeIntervalSince1970: 1_790_000_000)
        var ps = MockSessionStore.makePhotos(count: 6, firstID: 1, start: t)
        let times: [Double] = [0, 0, 0, 5, 5, 60]
        for i in ps.indices {
            ps[i].meta.captureDate = t.addingTimeInterval(times[i])
            ps[i].meta.shotNumber = 100 + i
            ps[i].meta.camera = "SONY ILCE-9"
        }
        return ps
    }

    @Test func dsaiBurstsFromMetadata() {
        let ps = burstPhotos()
        let g = BurstGrouping().groups(ps)
        #expect(g == [[ps[0].id, ps[1].id, ps[2].id], [ps[3].id, ps[4].id], [ps[5].id]])
        #expect(CaptureTimeGrouping().groups(ps).count == 3)
    }

    @Test func stateUsesGroupingAndBestHints() {
        let ps = burstPhotos()
        let s = StackState()
        s.update(photos: ps, grouping: BurstGrouping(), best: [ps[2].id])
        #expect(s.stacks.map(\.memberIDs) == [[ps[0].id, ps[1].id, ps[2].id], [ps[3].id, ps[4].id]])
        #expect(s.stacks[0].topID == ps[2].id, "ds-ai's best frame is on top")
        var rejected = ps
        rejected[2].pick = .rejected
        s.update(photos: rejected, grouping: BurstGrouping(), best: [ps[2].id])
        #expect(s.stacks[0].topID != ps[2].id, "never a rejected frame on top")
    }

    @Test func refreshIfNeededIsConstantTimeWhenUnchanged() {
        let ps = burstPhotos()
        let b = BrowserState(grouping: BurstGrouping())
        var evaluated = 0
        b.refreshIfNeeded(version: 1) { evaluated += 1; return ps }
        #expect(b.rebuilds == 1 && b.stacks.stacks.count == 2)
        for _ in 0..<100 { b.refreshIfNeeded(version: 1) { evaluated += 1; return ps } }
        #expect(evaluated == 1 && b.rebuilds == 1, "same version: photos never evaluated")
        b.refreshIfNeeded(version: 2) { evaluated += 1; return ps }
        #expect(evaluated == 2 && b.rebuilds == 1, "new version, same ids: no rebuild")
        b.refreshIfNeeded(version: 3) { Array(ps.prefix(3)) }
        #expect(b.rebuilds == 2 && b.stacks.stacks.count == 1)
        b.bestHints = [ps[1].id]
        b.refreshIfNeeded(version: 3) { Array(ps.prefix(3)) }
        #expect(b.rebuilds == 3 && b.stacks.stacks[0].topID == ps[1].id, "new hints force a rebuild")
        b.refresh(photos: Array(ps.prefix(3)))
        #expect(b.rebuilds == 3)
    }

    @Test func registryPicksDSAIInLiveMode() {
        let demo = makeModel()
        #expect(demo.browserState.grouping is CaptureTimeGrouping)
        let live = AppModel.make(mode: .live, settings: makeSettings(), defaults: UserDefaults(suiteName: testSuiteName())!)
        #expect(live.browserState.grouping is BurstGrouping)
    }
}

/// Live path: real A9 samples → ds-raw preview → Vision + ds-ai (skips without samples).
@Suite(.serialized) struct AILiveSampleTests {
    @Test func realSamplesCullThroughDSAI() async throws {
        let urls = sampleARWs()
        guard !urls.isEmpty else { return }
        var photos: [Photo] = []
        for (i, u) in urls.enumerated() { photos.append(try await RustRaw.photo(at: u, id: PhotoID(Int64(i + 1)))) }
        #expect(photos.allSatisfy { $0.meta.shotNumber != nil || $0.meta.fileSequence != nil },
                "ds-raw shot/file sequence reaches PhotoMeta (GroupItem.seq)")
        let images = PreviewThumbnailProvider(previews: RustPreviewProvider())
        let clock = ContinuousClock()
        let start = clock.now
        let out = try await VisionAIService(language: RulesEditService()).cull(photos, images: images) { _, _ in }
        let ms = (clock.now - start) / photos.count
        print("AI3 live cull: \(photos.count) samples, \(ms) per photo (Vision + ds-ai, debug)")
        #expect(out.count == photos.count)
        #expect(out.allSatisfy { $0.score >= 0 && $0.score <= 1 && !$0.reasons.contains("Could not analyze") })
        #expect(out.filter { $0.pick == .rejected }.allSatisfy { !$0.reasons.isEmpty })
        let groups = BurstGrouping().groups(photos)
        #expect(groups.flatMap { $0 }.count == photos.count)
        print("AI3 live bursts on samples: \(groups.map(\.count)) (distinct moments stay single)")
    }

    @Test func groupingTwoThousandIsFast() {
        var ps = MockSessionStore.makePhotos(count: 2000, firstID: 1, start: Date(timeIntervalSince1970: 1_790_000_000))
        for i in ps.indices {
            ps[i].meta.shotNumber = i
            ps[i].meta.captureDate = Date(timeIntervalSince1970: (ps[i].meta.captureDate.timeIntervalSince1970).rounded(.down))
        }
        var g: [[PhotoID]] = []
        let d = TestTiming.cpu { g = BurstGrouping().groups(ps) }
        print("AI3 ds-ai burst grouping, 2000 photos: \(d) CPU")
        #expect(g.flatMap { $0 }.count == 2000)
        #expect(d < .milliseconds(500))
    }
}
