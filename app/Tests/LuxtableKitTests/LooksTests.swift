import CoreGraphics
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// One-touch looks: structure, intensity, undo, the AI pick, and the effects' codec.
@Suite("Looks and effects")
struct LooksTests {
    /// A photo with its own correction.
    func corrected() -> EditState {
        var e = EditState()
        e.exposure.ev = 0.7
        e.exposure.contrast = 5
        e.hdr.shadows = 10
        e.detail.clarity = 12
        e.crop.rect = CGRect(x: 0.1, y: 0.1, width: 0.8, height: 0.8)
        return e
    }

    func scalarKeys(changed a: EditState, _ b: EditState) -> Set<String> {
        Set(EditCodec.scalars.filter { a[keyPath: $0.1] != b[keyPath: $0.1] }.map(\.0))
    }

    // MARK: Looks

    @Test func everyLookIsGenericChangesSomethingAndRecordsWhatItChanged() {
        let ids = LookPack.all.map(\.id)
        #expect(Set(ids).count == ids.count, "stable unique ids")
        #expect(Set(LookPack.all.map(\.preset.id)).count == ids.count)
        #expect(LookPack.all.count >= 18)
        let base = corrected()
        for look in LookPack.all {
            #expect(look.preset.category == LookPack.category && look.preset.isBuiltIn)
            #expect(!look.summary.isEmpty)
            let out = look.apply(to: base)
            let changed = scalarKeys(changed: base, out)
            #expect(!changed.isEmpty, "\(look.name) changes something")
            let rec = out.look
            #expect(rec?.id == look.id && rec?.intensity == 1)
            #expect(Set(rec?.to.keys.map { $0 } ?? []) == changed, "\(look.name) records exactly what it changed")
            #expect(look.preset.edit.curve.points.isEmpty, "\(look.name): scalars only, so intensity is exact")
            // Rides on the photo's own exposure; crop and layers untouched.
            #expect(out.exposure.ev == base.exposure.ev + look.preset.edit.exposure.ev)
            #expect(out.crop == base.crop && out.layers == base.layers)
            #expect(LookPack.look(id: look.id) == look && LookPack.look(preset: look.preset.id) == look)
        }
        #expect(LookPack.look(id: "look.none") == nil)
    }

    @Test func applyingALookTwiceIsTheSameAsOnce() {
        let base = corrected()
        for look in LookPack.all {
            let once = look.apply(to: base)
            let twice = look.apply(to: once)
            #expect(scalarKeys(changed: base, twice) == scalarKeys(changed: base, once), "\(look.name) structure")
            #expect(twice == once, "\(look.name) idempotent")
        }
        // Stacked (⇧-click), another look on top starts from the first one's values.
        let cine = LookPack.look(id: "look.cinematic")!, noir = LookPack.look(id: "look.noir")!
        let both = noir.apply(to: cine.apply(to: base), stacking: true)
        #expect(both.look?.id == noir.id && both.exposure.saturation == -100)
        #expect(both.colorBalance.shadows.saturation == cine.preset.edit.colorBalance.shadows.saturation)
        // Stacking the look already applied is still the same as once.
        #expect(cine.apply(to: cine.apply(to: base), stacking: true) == cine.apply(to: base))
    }

    @Test func anotherLookReplacesTheFirst() {
        let base = corrected()
        for a in LookPack.all {
            for b in LookPack.all where b.id != a.id {
                #expect(b.apply(to: a.apply(to: base)) == b.apply(to: base), "\(b.name) replaces \(a.name)")
            }
        }
        // A field the user changed after the first look is theirs: replacing keeps it.
        let cine = LookPack.look(id: "look.cinematic")!, pop = LookPack.look(id: "look.pop")!
        var e = cine.apply(to: base)
        e.vignette.amountEV = -1 // Cinematic set −0.3; Pop leaves the vignette alone
        let out = pop.apply(to: e)
        #expect(out.look?.id == pop.id)
        #expect(out.vignette.amountEV == -1, "their value is theirs")
        #expect(out.hdr.highlights == base.hdr.highlights, "the first look's untouched fields restore")
        #expect(out.colorBalance.shadows.saturation == base.colorBalance.shadows.saturation, "the first look's grade is gone")
        // Intensity 0 on the second look returns to the photo before either look.
        var zero = LookIntensity.set(0, on: pop.apply(to: cine.apply(to: base)))
        zero.look = nil
        #expect(zero == base)
    }

    /// The panel: a click replaces the photo's look, ⇧-click (Add on Top) stacks; one undo step each.
    @MainActor
    @Test func theModelReplacesOrStacks() async {
        let m = makeModel()
        await m.start()
        let before = m.focusedEdit
        let cine = LookPack.look(id: "look.cinematic")!, noir = LookPack.look(id: "look.noir")!
        m.applyLook(cine)
        m.applyLook(noir)
        #expect(m.focusedEdit == noir.apply(to: before))
        m.undo()
        #expect(m.focusedEdit == cine.apply(to: before))
        m.applyLook(noir, stacking: true)
        #expect(m.focusedEdit == noir.apply(to: cine.apply(to: before), stacking: true))
        // The hover preview and the row thumbnails show what a plain click does: replace.
        let stacked = m.focusedEdit
        #expect(m.previewEdit(applying: cine.preset) == cine.apply(to: stacked))
        #expect(LookPack.applying(cine.preset, to: stacked) == cine.apply(to: stacked))
        #expect(cine.apply(to: stacked) != cine.apply(to: stacked, stacking: true))
    }

    @Test func intensityZeroIsTheIdentityAndScalesInBetween() {
        let base = corrected()
        for look in LookPack.all {
            let full = look.apply(to: base)
            var zero = LookIntensity.set(0, on: full)
            #expect(zero.look?.intensity == 0)
            zero.look = nil
            #expect(zero == base, "\(look.name) at 0 restores the photo")
            let back = LookIntensity.set(1, on: LookIntensity.set(0, on: full))
            #expect(back == full, "\(look.name) back to 1")
        }
        let pop = LookPack.look(id: "look.pop")!
        let half = LookIntensity.set(0.5, on: pop.apply(to: base))
        #expect(abs(half.exposure.contrast - (base.exposure.contrast + 7.5)) < 1e-9)
        #expect(abs(half.presence.vibrance - 10) < 1e-9)
        // Choices switch instead of scaling: tilt-shift stays a band down to just above 0.
        let tilt = LookPack.look(id: "look.tilt-shift")!.apply(to: base)
        #expect(LookIntensity.set(0.1, on: tilt).effects.lensBlur.mode == .plane)
        #expect(LookIntensity.set(0, on: tilt).effects.lensBlur.mode == .depth)
        #expect(abs(LookIntensity.set(0.5, on: tilt).effects.lensBlur.amount - 30) < 1e-9)
        // Clamped, and a no-op without a look.
        #expect(LookIntensity.set(7, on: tilt).look?.intensity == 1)
        #expect(LookIntensity.set(0.3, on: base) == base)
    }

    @Test func aFieldTheUserChangedKeepsTheirChange() {
        let base = corrected()
        let pop = LookPack.look(id: "look.pop")!
        var e = pop.apply(to: base)
        e.exposure.contrast += 10 // the user's own tweak after the look
        let zero = LookIntensity.set(0, on: e)
        #expect(zero.exposure.contrast == e.exposure.contrast, "their value is theirs")
        #expect(zero.detail.clarity == base.detail.clarity, "untouched fields restore exactly")
        // A value that only went through single precision still counts as untouched.
        var rounded = pop.apply(to: base)
        rounded.presence.vibrance = Double(Float(rounded.presence.vibrance + 1e-6))
        #expect(LookIntensity.set(0, on: rounded).presence.vibrance == base.presence.vibrance)
    }

    /// H2: a group reset (or pasted, or set by a style or the AI) after a look is no longer
    /// the look's: Intensity leaves it as it is instead of adding the look's share.
    @MainActor
    @Test func aResetGroupIsNoLongerTheLooks() async {
        let m = makeModel()
        await m.start()
        let before = m.focusedEdit
        m.applyLook(LookPack.look(id: "look.vintage")!)
        m.resetGroup(.vignette)
        m.resetGroup(.effects)
        let rec = m.focusedEdit.look
        #expect(rec?.to["vignette.amountEV"] == nil && rec?.to["fx.grain.amount"] == nil && rec?.from["fx.grain.size"] == nil)
        #expect(rec?.to["exposure.saturation"] != nil, "the rest is still the look's")
        m.setLookIntensity(0)
        var e = m.focusedEdit
        #expect(e.vignette.amountEV == 0 && e.effects.grain.amount == 0 && e.effects.grain.size == GrainEdit().size,
                "no +0.5 EV vignette, no −30 grain")
        e.look = nil
        e.vignette = before.vignette
        e.effects = before.effects
        #expect(e == before, "everything else restored")
        // Resetting every group it touched leaves no look.
        m.setLookIntensity(1)
        for g in [AdjustGroup.levels, .exposure, .colorBalance] { m.resetGroup(g) }
        #expect(m.focusedEdit.look == nil)
    }

    @Test func pastesStylesAndAIEditsTakeOverTheFieldsTheySet() {
        let base = corrected()
        let vintage = LookPack.look(id: "look.vintage")!.apply(to: base)
        // An absolute paste replaces; a relative one adds to the look's share.
        let clip = AdjustmentClipboard(source: EditState())
        let pasted = clip.apply(to: vintage, options: PasteOptions(groups: [.vignette, .exposure], relative: [.exposure]))
        #expect(pasted.look?.to["vignette.amountEV"] == nil)
        #expect(pasted.look?.to["exposure.saturation"] == vintage.look?.to["exposure.saturation"])
        #expect(LookIntensity.set(0, on: pasted).vignette.amountEV == 0)
        // A style takes over only the fields it sets.
        var styleEdit = EditState()
        styleEdit.effects.grain.amount = 5
        let style = Preset(name: "Grain", groups: [.effects], edit: styleEdit)
        let styled = style.apply(to: vintage)
        #expect(styled.look?.to["fx.grain.amount"] == nil && styled.look?.to["fx.grain.size"] != nil)
        #expect(LookIntensity.set(0, on: styled).effects.grain.amount == 5)
        // An AI edit, the same.
        var ai = vintage
        ai.exposure.saturation = 10
        let merged = AIEditMerge.apply(result: ai, base: vintage, current: vintage)
        #expect(merged.look?.to["exposure.saturation"] == nil && merged.look?.to["exposure.contrast"] != nil)
        #expect(LookIntensity.set(0, on: merged).exposure.saturation == 10)
        #expect(AIEditMerge.apply(result: vintage, base: vintage, current: vintage).look == vintage.look)
        // A wheel's hue and saturation go together: the AI changing only the saturation
        // takes the hue too (else the wheel would move half by the look).
        var tint = vintage
        tint.colorBalance.master.saturation = 0.3
        let wheel = AIEditMerge.apply(result: tint, base: vintage, current: vintage).look
        #expect(wheel?.to["cb.master.hue"] == nil && wheel?.to["cb.master.saturation"] == nil)
        #expect(LookIntensity.set(0, on: AIEditMerge.apply(result: tint, base: vintage, current: vintage)).colorBalance.master
            == tint.colorBalance.master)
    }

    /// A field the user changed after the look is theirs: Intensity leaves it as they set
    /// it, and a round trip of the slider (1 → 0 → 1) never loses it.
    @Test func aChangedFieldIsTheirsAndSurvivesARoundTrip() {
        let base = corrected()
        var v = LookPack.look(id: "look.vintage")!.apply(to: base) // grain 30, out black 0.06
        v.effects.grain.amount = 10
        v.levels.outBlack = 0.01
        let zero = LookIntensity.set(0, on: v)
        #expect(zero.effects.grain.amount == 10 && zero.levels.outBlack == 0.01, "theirs, as set")
        #expect(zero.look?.to["fx.grain.amount"] == nil && zero.look?.from["levels.outBlack"] == nil)
        #expect(zero.effects.grain.size == GrainEdit().size, "the look's own fields still go")
        let back = LookIntensity.set(1, on: zero)
        #expect(back.effects.grain.amount == 10 && back.levels.outBlack == 0.01, "1 → 0 → 1 keeps the user's 10")
        #expect(back.effects.grain.size == v.effects.grain.size)
        var e = LookPack.look(id: "look.pop")!.apply(to: base) // saturation +20
        e.exposure.saturation = -100
        #expect(LookIntensity.set(0, on: e).exposure.saturation == -100)
        // A choice the user changed is theirs too.
        var tilt = LookPack.look(id: "look.tilt-shift")!.apply(to: base)
        tilt.effects.lensBlur.mode = .depth
        let t0 = LookIntensity.set(0, on: tilt)
        #expect(t0.effects.lensBlur.mode == .depth && t0.look?.to["fx.blur.mode"] == nil)
        // Every field theirs: the look owns nothing, so it's gone (no Intensity row).
        var all = LookPack.look(id: "look.pop")!.apply(to: base)
        all.exposure.saturation += 1
        all.presence.vibrance += 1
        all.exposure.contrast += 1
        all.detail.clarity += 1
        let none = LookIntensity.set(0.5, on: all)
        #expect(none.look == nil)
        #expect(LookIntensity.set(0.2, on: none) == none, "nothing left to move")
        #expect(none.exposure == all.exposure && none.presence == all.presence)
    }

    /// Med 1: a colour-balance wheel fades across the wheel: at a low intensity the look's
    /// hue is a tint over the photo's own, not a snap to the look's hue.
    @Test func colourBalanceFadesInsteadOfSnappingItsHue() {
        var base = corrected()
        base.colorBalance.shadows = WheelValue(hue: 0, saturation: 0.2, lightness: 0.1)
        let cine = LookPack.look(id: "look.cinematic")! // shadows 190°, 0.16
        let full = cine.apply(to: base)
        #expect(full.colorBalance.shadows.hue == 190 && full.colorBalance.shadows.saturation == 0.16)
        let low = LookIntensity.set(0.1, on: full).colorBalance.shadows
        let toward = min(low.hue, 360 - low.hue)
        #expect(toward < 2, "still about the photo's red: \(low.hue)")
        #expect(abs(low.saturation - 0.1643) < 1e-3)
        #expect(low.lightness == 0.1)
        let zero = LookIntensity.set(0, on: full).colorBalance.shadows
        #expect(zero == base.colorBalance.shadows, "0 restores exactly")
        #expect(LookIntensity.set(1, on: LookIntensity.set(0.4, on: full)).colorBalance == full.colorBalance)
        // From a neutral wheel: the look's hue at once, a share of its saturation.
        let neutral = cine.apply(to: corrected())
        let n = LookIntensity.set(0.25, on: neutral).colorBalance.shadows
        #expect(abs(n.hue - 190) < 1e-6 && abs(n.saturation - 0.04) < 1e-9)
        #expect(LookIntensity.set(0, on: neutral).colorBalance.shadows == WheelValue())
        // Halfway down and back keeps the look's colour where it was.
        let half = LookIntensity.set(0.5, on: full)
        let again = LookIntensity.set(0.5, on: LookIntensity.set(0.2, on: half)).colorBalance
        for (w, h) in [(again.shadows, half.colorBalance.shadows), (again.highlights, half.colorBalance.highlights)] {
            #expect(abs(w.hue - h.hue) < 1e-6 && abs(w.saturation - h.saturation) < 1e-9)
        }
        // The user's own change to the wheel rides along.
        var mine = full
        mine.colorBalance.shadows.saturation = 0.3
        let back = LookIntensity.set(0, on: mine)
        #expect(back.colorBalance.shadows == mine.colorBalance.shadows, "the user's wheel is theirs")
        #expect(back.look?.to["cb.shadows.hue"] == nil && back.look?.to["cb.shadows.saturation"] == nil)
        #expect(back.colorBalance.highlights == base.colorBalance.highlights, "the look's other wheel still goes")
        // A wheel the look doesn't touch is left alone.
        #expect(LookIntensity.set(0.3, on: full).colorBalance.master == full.colorBalance.master)
        var opposite = full
        opposite.look?.from["cb.shadows.hue"] = 10
        opposite.colorBalance.shadows = WheelValue(hue: 10, saturation: 0.16, lightness: 0)
        opposite.look?.from["cb.shadows.saturation"] = 0.16
        opposite.look?.intensity = 0
        opposite.look?.to["cb.shadows.hue"] = 190
        let mid = LookIntensity.set(0.5, on: opposite).colorBalance.shadows
        #expect(mid.saturation < 1e-6 && mid.hue == 10, "through the centre: no hue, the old one kept")
    }

    /// Med 2: hovering a look's row (and its tile) shows what a click leaves, also when the
    /// look is already applied (not the look twice).
    @MainActor
    @Test func hoverAndTilesShowWhatAClickLeaves() async {
        let m = makeModel()
        await m.start()
        let cine = LookPack.look(id: "look.cinematic")!
        m.applyLook(cine)
        let preview = m.previewEdit(applying: cine.preset)
        let applied = m.focusedEdit
        m.applyLook(cine)
        #expect(preview == m.focusedEdit && m.focusedEdit == applied)
        let style = Preset(name: "Warm", groups: [.exposure], edit: { var e = EditState(); e.exposure.contrast = 3; return e }())
        #expect(m.previewEdit(applying: style) == style.apply(to: m.focusedEdit))
        #expect(LookPack.applying(cine.preset, to: applied) == cine.apply(to: applied))
    }

    /// Med 3: a look's effects are all its own, also the settings it leaves at their defaults.
    @Test func aLookReplacesTheEffectsItUses() {
        let base = corrected()
        let tilt = LookPack.look(id: "look.tilt-shift")!, portrait = LookPack.look(id: "look.portrait")!
        let p = portrait.apply(to: tilt.apply(to: base))
        #expect(p.effects.lensBlur.mode == .depth && p.effects.lensBlur.highlightBoost == 30 && p.effects.lensBlur.amount == 45)
        #expect(p == portrait.apply(to: portrait.apply(to: tilt.apply(to: base))))
        let noir = LookPack.look(id: "look.noir")!, warm = LookPack.look(id: "look.warm-negative")!
        let w = warm.apply(to: noir.apply(to: base), stacking: true)
        #expect(w.effects.grain.size == GrainEdit().size && w.effects.grain.roughness == GrainEdit().roughness)
        #expect(LookIntensity.set(0, on: w).effects.grain == noir.apply(to: base).effects.grain, "stacked: 0 restores Noir's grain")
        let replaced = warm.apply(to: noir.apply(to: base))
        #expect(LookIntensity.set(0, on: replaced).effects.grain == base.effects.grain, "replaced: 0 is the photo's own grain")
        // Effects a look doesn't use stay the photo's.
        var glowing = base
        glowing.effects.glow.amount = 20
        #expect(LookPack.look(id: "look.pop")!.apply(to: glowing).effects.glow.amount == 20)
        #expect(LookPack.effectFields(EditState()).isEmpty)
    }

    /// Med 6 / low: Intensity follows Edit All Selected; a look at 0 is no edit.
    @MainActor
    @Test func intensityFollowsTheSelectionAndZeroIsNoEdit() async {
        let m = makeModel()
        await m.start()
        m.perform(.move(.right, extend: true))
        m.editAllSelected = true
        m.applyLook(LookPack.look(id: "look.pop")!)
        #expect(m.selectedIDs.count == 2 && m.selectedIDs.allSatisfy { m.edit(for: $0).look?.id == "look.pop" })
        m.setLookIntensity(0.5)
        m.endEditGesture()
        #expect(m.selectedIDs.allSatisfy { m.edit(for: $0).look?.intensity == 0.5 })
        m.setLookIntensity(0)
        m.endEditGesture()
        let untouched = m.selectedIDs.filter { m.edit(for: $0).isIdentity }
        #expect(!untouched.isEmpty, "a photo that had no edit before the look is unedited at 0")
        #expect(untouched.allSatisfy { m.photo($0)?.isEdited == false })
        var e = EditState()
        e.look = AppliedLookEdit(id: "look.pop", intensity: 0.2)
        #expect(!e.isIdentity)
    }

    @MainActor
    @Test func applyAndIntensityDragAreOneUndoStepEach() async {
        let m = makeModel()
        await m.start()
        let before = m.focusedEdit
        let depth = m.history.undoStack.count
        let cine = LookPack.look(id: "look.cinematic")!
        #expect(m.previewEdit(applying: cine)?.look?.id == cine.id)
        #expect(m.focusedEdit == before, "hover doesn't edit")
        m.applyLook(cine)
        #expect(m.history.undoStack.count == depth + 1 && m.history.undoLabel == "Look Cinematic")
        #expect(m.focusedEdit.look?.id == cine.id)
        for t in [0.9, 0.7, 0.5, 0.3, 0.2] { m.setLookIntensity(t) }
        m.endEditGesture()
        #expect(m.history.undoStack.count == depth + 2, "a drag is one step")
        #expect(abs((m.focusedEdit.look?.intensity ?? 0) - 0.2) < 1e-9)
        m.undo()
        #expect(m.focusedEdit.look?.intensity == 1)
        m.undo()
        #expect(m.focusedEdit == before)
        m.setLookIntensity(0.5) // no look: nothing happens
        #expect(m.history.undoStack.count == depth)
        m.focus(nil, extend: false)
        #expect(m.previewEdit(applying: cine) == nil)
    }

    // MARK: AI pick

    @Test func suggestionsFollowTheScene() {
        func ids(_ s: LookSignals) -> [String] { LookSuggester.suggest(s).map(\.look.id) }
        var person = LookSignals()
        person.faces = 1
        person.faceArea = 0.06
        let p = LookSuggester.suggest(person)
        #expect(p.first?.look.id == "look.portrait" && p.first?.reason.contains("6%") == true)
        #expect((3...4).contains(p.count))

        var night = LookSignals()
        night.tags = [.night]
        #expect(Array(ids(night).prefix(2)) == ["look.moody", "look.noir"])
        #expect(LookSuggester.suggest(night)[0].reason.hasPrefix("A night scene"))
        var iso = LookSignals()
        iso.iso = 6400
        #expect(LookSuggester.suggest(iso)[0].reason.contains("ISO 6400"))
        var slow = LookSignals()
        slow.shutter = 0.5
        #expect(LookSuggester.suggest(slow)[0].reason.contains("slow shutter"))
        var dark = LookSignals()
        dark.medianLuma = 0.1
        #expect(LookSuggester.suggest(dark)[0].reason.hasPrefix("A dark photo"))

        var land = LookSignals()
        land.tags = [.landscape]
        land.skyFraction = 0.4
        #expect(ids(land) == ["look.hdr-detail", "look.golden-hour", "look.pop"])
        land.skyFraction = 0
        #expect(ids(land).prefix(2) == ["look.golden-hour", "look.pop"])

        var sunset = LookSignals()
        sunset.tags = [.sunset, .landscape]
        #expect(ids(sunset).first == "look.golden-hour")
        var snow = LookSignals()
        snow.tags = [.snow]
        #expect(ids(snow).first == "look.bright-airy")
        snow.tags = [.beach]
        #expect(LookSuggester.suggest(snow)[0].reason.contains("beach"))
        var back = LookSignals()
        back.tags = [.backlit]
        #expect(ids(back).first == "look.dreamy-glow")

        let plain = LookSuggester.suggest(LookSignals())
        #expect(plain.map(\.look.id) == ["look.cinematic", "look.pop", "look.faded-matte"])
        // Every suggestion is unique, has a reason, and at most four are shown.
        var all = LookSignals()
        all.tags = Set(OptimizeSceneTag.allCases)
        all.faces = 2
        all.iso = 12800
        let many = LookSuggester.suggest(all)
        #expect(many.count == 4 && Set(many.map(\.id)).count == 4 && many.allSatisfy { !$0.reason.isEmpty })
    }

    @Test func signalsReadExifAndTheHistogram() {
        var s = OptimizeSignals()
        s.tags = [.portrait]
        s.faces = 1
        var h = Histogram()
        h = Histogram(red: h.red, green: h.green, blue: h.blue,
                      luma: (0..<256).map { $0 == 40 ? 100 : 0 }, shadowClipped: 0, highlightClipped: 0, pixelCount: 100)
        let sig = LookSignals(s, meta: samplePhoto().meta, histogram: h)
        #expect(sig.iso == 200 && sig.faces == 1 && abs((sig.medianLuma ?? 0) - 40.0 / 255) < 1e-9)
        #expect(sig.lowLight, "a median at 16% renders dark")
        #expect(LookSignals.median(Histogram()) == nil)
        #expect(LookSignals(OptimizeSignals(), meta: nil, histogram: nil).medianLuma == nil)
    }

    @MainActor
    @Test func theFocusedPhotoGetsSuggestions() async {
        let m = makeModel()
        await m.start()
        m.optimizeSignals = { _ in
            var s = OptimizeSignals()
            s.tags = [.sunset]
            return s
        }
        #expect(await m.lookSuggestions().first?.look.id == "look.golden-hour")
        m.focus(nil, extend: false)
        #expect(await m.lookSuggestions().isEmpty)
    }

    /// Med 5: one analysis per photo, and an answer that lands after the focus moved on is
    /// dropped (it is the old photo's).
    @MainActor
    @Test func suggestionsAreCachedPerPhotoAndNeverStale() async throws {
        let m = makeModel()
        await m.start()
        let first = try #require(m.focusedID)
        var calls = 0
        var moveOn: PhotoID?
        m.optimizeSignals = { _ in
            calls += 1
            if let next = moveOn { m.focus(next, extend: false) }
            var s = OptimizeSignals()
            s.tags = [.sunset]
            return s
        }
        _ = await m.lookSuggestions()
        _ = await m.lookSuggestions()
        #expect(calls == 1, "analysed once")
        let second = try #require(m.photos.first { $0.id != first && !$0.isVideo }?.id)
        m.focus(second, extend: false)
        moveOn = first // the focus moves back while the second photo is analysed
        #expect(await m.lookSuggestions().isEmpty, "the second photo's looks aren't shown on the first")
        #expect(calls == 2 && m.focusedID == first)
        #expect(await m.lookSuggestions().first?.look.id == "look.golden-hour" && calls == 2)
    }

    // MARK: Effects model and codec

    @Test func effectsRoundTripThroughTheSidecarLosslessly() throws {
        var e = corrected()
        e.effects.lensBlur.amount = 55
        e.effects.lensBlur.mode = .plane
        e.effects.lensBlur.focus = CGPoint(x: 0.25, y: 0.75)
        e.effects.lensBlur.focalRange = 7
        e.effects.lensBlur.highlightBoost = 80
        e.effects.lensBlur.shape = .catEye
        e.effects.lensBlur.band.center = CGPoint(x: 0.4, y: 0.6)
        e.effects.lensBlur.band.angleDeg = 12
        e.effects.lensBlur.band.width = 0.1
        e.effects.lensBlur.band.feather = 0.3
        e.effects.glow = GlowEdit()
        e.effects.glow.amount = 33
        e.effects.grain.amount = 20
        e.effects.grain.size = 44
        e.effects.grain.roughness = 70
        e.effects.halation.amount = 15
        e.look = AppliedLookEdit(id: "look.pop", intensity: 0.4, from: ["exposure.contrast": 5], to: ["exposure.contrast": 20])
        let base: AdjustmentsCodec.JSON = ["effects": ["future": 1, "lens_blur": ["future": "x"]], "zz": true]
        let json = try AdjustmentsCodec.encode(e, onto: base)
        let (back, raw) = try AdjustmentsCodec.decode(json)
        #expect(back.effects == e.effects && back.look == e.look)
        #expect((raw["effects"] as? [String: Any])?["future"] as? Int == 1, "unknown keys kept")
        #expect(((raw["effects"] as? [String: Any])?["lens_blur"] as? [String: Any])?["shape"] as? String == "cat_eye")
        #expect(raw["zz"] as? Bool == true)
        // Cleared: the focus goes back to "subject" (no key), the look record to null.
        var cleared = back
        cleared.effects.lensBlur.focus = nil
        cleared.look = nil
        let json2 = try AdjustmentsCodec.encode(cleared, onto: raw)
        let (again, raw2) = try AdjustmentsCodec.decode(json2)
        #expect(again.effects.lensBlur.focus == nil && again.look == nil)
        #expect(raw2["look"] is NSNull)
        #expect(((raw2["effects"] as? [String: Any])?["lens_blur"] as? [String: Any])?["focus"] == nil)
        // Older sidecars: defaults.
        let (old, _) = try AdjustmentsCodec.decode("{}")
        #expect(old.effects == EffectsEdit() && old.look == nil)
        let (bad, badRaw) = try AdjustmentsCodec.decode(#"{"effects":{"lens_blur":{"mode":"warp","shape":"anamorphic"}},"look":{"id":""}}"#)
        #expect(bad.effects.lensBlur.mode == .newer && bad.effects.lensBlur.shape == .circle && bad.look == nil)
        // A newer build's mode shows as such: no blur here, so no depth map is asked for.
        var newer = bad
        newer.effects.lensBlur.amount = 50
        #expect(!newer.effects.lensBlur.needsDepth && BlurMode.newer.title == "Newer")
        #expect(BlurMode.pickable == [.depth, .plane] && BlurMode.read("_newer") == .newer && BlurMode.read("plane") == .plane)
        let (nullMode, _) = try AdjustmentsCodec.decode(#"{"effects":{"lens_blur":{"mode":null}}}"#)
        #expect(nullMode.effects.lensBlur.mode == .newer, "not a name: a newer build's too")
        let (none, _) = try AdjustmentsCodec.decode(#"{"effects":{"lens_blur":{}}}"#)
        #expect(none.effects.lensBlur.mode == .depth, "no mode: the default")
        // A newer build's choices are written back as they were, unless the user picks one.
        let kept = AdjustmentsCodec.overlay(bad, onto: badRaw)
        let keptBlur = (kept["effects"] as? [String: Any])?["lens_blur"] as? [String: Any]
        #expect(keptBlur?["mode"] as? String == "warp" && keptBlur?["shape"] as? String == "anamorphic")
        var picked = bad
        picked.effects.lensBlur.mode = .plane
        picked.effects.lensBlur.shape = .catEye
        let pickedBlur = (AdjustmentsCodec.overlay(picked, onto: badRaw)["effects"] as? [String: Any])?["lens_blur"] as? [String: Any]
        #expect(pickedBlur?["mode"] as? String == "plane" && pickedBlur?["shape"] as? String == "cat_eye")
    }

    @Test func effectsAreAGroupWithCodecKeys() {
        var e = EditState()
        #expect(e.effects.isOff && !e.effects.lensBlur.needsDepth)
        e.effects.grain.amount = 10
        #expect(AdjustGroup.changed(in: e) == [.effects] && !e.effects.isOff)
        #expect(AdjustGroup.effects.title == "Effects" && !AdjustGroup.effects.supportsRelative && !AdjustGroup.effects.isOptIn)
        let pasted = AdjustmentClipboard(source: e).apply(to: EditState(), options: PasteOptions(groups: [.effects]))
        #expect(pasted.effects == e.effects)
        #expect(EditCodec.group(of: "fx.grain.amount") == .effects)
        #expect(EditCodec.isDiscrete("fx.blur.mode") && !EditCodec.isDiscrete("cb.master.hue") && !EditCodec.isDiscrete("fx.blur.amount"))
        #expect(EditCodec.isWheelPolar("cb.master.hue") && EditCodec.isWheelPolar("cb.shadows.saturation"))
        #expect(!EditCodec.isWheelPolar("cb.master.lightness") && !EditCodec.isWheelPolar("ce.1.saturation"))
        var b = LensBlurEdit()
        b.modeIndex = 1
        b.shapeIndex = 9
        #expect(b.mode == .plane && b.shape == .catEye && b.modeIndex == 1 && b.shapeIndex == 2)
        b.amount = 1
        #expect(!b.needsDepth)
        b.mode = .depth
        #expect(b.needsDepth)
        var band = FocusBandEdit()
        band.centerX = 0.2
        band.centerY = 0.3
        #expect(band.center == CGPoint(x: 0.2, y: 0.3))
        #expect(BlurMode.allCases.allSatisfy { !$0.title.isEmpty } && BokehShape.allCases.allSatisfy { !$0.title.isEmpty })
        // Snapshot (preset files) round trip of the effects.
        let snap = EditCodec.decode(EditCodec.encode(e))
        #expect(snap.effects == e.effects)
        // AI merges carry the effects.
        #expect(AIEditMerge.apply(result: e, base: EditState(), current: EditState()).effects == e.effects)
    }

    @Test func amountReadsAsAnFStop() {
        #expect(abs(LensBlurEdit.fNumber(amount: 0) - 16) < 1e-9)
        #expect(abs(LensBlurEdit.fNumber(amount: 100) - 1.2) < 1e-9)
        #expect(abs(LensBlurEdit.amount(fNumber: LensBlurEdit.fNumber(amount: 37)) - 37) < 1e-9)
        #expect(ValueFormat.fStop.string(0) == "Off")
        #expect(ValueFormat.fStop.string(100) == "f/1.2")
        #expect(Specs.blurAmount.parse("off") == 0)
        let f28 = Specs.blurAmount.parse("f/2.8")
        #expect(abs(LensBlurEdit.fNumber(amount: f28 ?? 0) - 2.8) < 0.1)
        #expect(Specs.blurAmount.parse("50") == 50)
        #expect(Specs.focalRange.defaultValue == LensBlurEdit().focalRange)
        #expect(Specs.bandPosition.defaultValue == Double(FocusBandEdit().center.y))
    }

    @Test func aFocusClickSetsTheFocusPoint() {
        let d = CanvasDrag(tool: .focusPoint, start: CGPoint(x: 0.3, y: 0.6))
        #expect(CanvasInteraction.commit(d, imageSize: CGSize(width: 600, height: 400), cropAspect: nil) == .pickFocus(CGPoint(x: 0.3, y: 0.6)))
        #expect(LensBlurEdit.focusPoint(CGPoint(x: -0.2, y: 1.4)) == CGPoint(x: 0, y: 1))
        #expect(!CanvasTool.focusPoint.editsMask && CanvasTool.focusPoint.viewerSymbol == "scope")
    }

    @MainActor
    @Test func theFocusPickerSetsTheFocusInOneStep() async {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 600, height: 400), backingScale: 2)
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        let id = m.focusedID!
        m.wbPickerActive = true
        m.toggleFocusPicker()
        #expect(m.focusPickerActive && !m.wbPickerActive)
        #expect(m.viewer.activeTool == .focusPoint && m.viewer.developedGeometry == m.viewer.capabilities.rendersGeometry)
        let depth = m.history.undoStack.count
        m.viewer.beginCanvasDrag(at: CGPoint(x: 300, y: 200))
        let commit = m.viewer.endCanvasDrag()
        guard case .pickFocus = commit else {
            Issue.record("expected a focus pick, got \(commit)")
            return
        }
        #expect(m.history.undoStack.count == depth + 1 && m.history.undoLabel == "Lens Blur Focus")
        let f = m.edit(for: id).effects.lensBlur.focus
        #expect(f != nil && f.map { (0...1).contains($0.x) && (0...1).contains($0.y) } == true)
        // A tilt-shift band has no depth to pick: the canvas isn't a focus picker then.
        m.updateEdit(label: "Lens Blur Mode") { $0.effects.lensBlur.mode = .plane }
        #expect(m.focusPickerActive && m.viewer.activeTool != .focusPoint)
        m.updateEdit(label: "Lens Blur Mode") { $0.effects.lensBlur.mode = .depth }
        #expect(m.viewer.activeTool == .focusPoint)
        m.perform(.escape)
        #expect(!m.focusPickerActive)
    }
}

/// The final review's reproductions (H2, Med 1, Med 3), kept as written.
@Suite("Review: looks")
struct ReviewLooksTests {
    @Test func intensityAfterAGroupResetStaysInRange() {
        let vintage = LookPack.look(id: "look.vintage")!
        var e = vintage.apply(to: EditState())
        e.vignette = EditState().vignette   // the user resets Vignetting
        e.effects = EditState().effects     // and Effects
        let off = LookIntensity.set(0, on: e)
        #expect(off.vignette.amountEV <= 0, "vignette \(off.vignette.amountEV)")
        #expect(off.effects.grain.amount >= 0, "grain \(off.effects.grain.amount)")
    }

    @Test func cinematicAtLowIntensityKeepsTheShadowHue() {
        var e = EditState()
        e.colorBalance.shadows = WheelValue(hue: 30, saturation: 0.15, lightness: 0)
        let low = LookIntensity.set(0.05, on: LookPack.look(id: "look.cinematic")!.apply(to: e))
        #expect(abs(low.colorBalance.shadows.hue - 30) < 30, "hue \(low.colorBalance.shadows.hue)")
    }

    @Test func portraitAfterTiltShiftIsADepthBlur() {
        let tilt = LookPack.look(id: "look.tilt-shift")!.apply(to: EditState())
        let portrait = LookPack.look(id: "look.portrait")!.apply(to: tilt)
        #expect(portrait.effects.lensBlur.mode == .depth)
    }
}
