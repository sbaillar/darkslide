import CoreGraphics
import Foundation
import Testing
@testable import DarkslideKit

/// Capture One–style Copy and Apply: one-click Copy of the edited groups (or the Custom
/// ticks), the clipboard kept across photos, Apply onto the selection but the copied photo
/// as one undo step.
@MainActor
@Suite struct CopyApplyTests {
    /// The first photo edited in exposure, HDR, crop and layers; focus on it.
    func loaded() async -> AppModel {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "setup") {
            $0.exposure.ev = 0.5
            $0.hdr.shadows = 20
            $0.crop.angle = 2
            $0.crop.rect = CGRect(x: 0.1, y: 0.1, width: 0.8, height: 0.8)
            $0.layers = [LayerEdit(id: 0, name: "Sky"), LayerEdit(id: 1, name: "Face")]
        }
        return m
    }

    @Test func oneClickCopyTakesTheEditedGroupsWithoutCrop() async {
        let m = await loaded()
        let src = m.focusedPhoto!
        #expect(m.settings.copyMode == .edited)
        m.perform(.copyAdjustments)
        #expect(m.sheet == nil, "no dialog")
        #expect(m.clipboard?.sourceID == src.id && m.clipboard?.source.exposure.ev == 0.5)
        #expect(m.clipboard?.sourceName == src.filename && m.clipboard?.sourceSize == src.pixelSize, "captured at Copy")
        #expect(m.pasteOptions.groups == [.exposure, .hdr], "edited groups, not crop or layers")
        #expect(m.toast?.message == "Copied 2 adjustments from \(src.filename)")
        let target = m.visibleIDs[1]
        m.focus(target, extend: false)
        #expect(m.applyToSelection() == 1)
        let e = m.edit(for: target)
        #expect(e.exposure.ev == 0.5 && e.hdr.shadows == 20)
        #expect(e.crop == CropEdit() && e.layers.isEmpty, "crop, straighten and layers stay per photo")
    }

    /// The reviewer's H1: a WB-only source must not reset the targets' own work.
    @Test func targetsKeepTheirOwnEditsInGroupsTheSourceNeverTouched() async {
        let m = makeModel()
        await m.start()
        let ids = Array(m.visibleIDs.prefix(4))
        m.updateEdits(Array(ids.dropFirst()), label: "own work") { _, e in
            e.exposure.ev = -0.7
            e.curve.points = [.rgb: [CurvePoint(0, 0.1), CurvePoint(1, 0.9)]]
            e.profile = .vivid
        }
        let own = m.edit(for: ids[1])
        m.updateEdit(label: "wb") { $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 6400, tint: 2)) }
        m.perform(.copyAdjustments)
        m.selectAll()
        m.perform(.applyAdjustments)
        for id in ids.dropFirst() {
            let e = m.edit(for: id)
            #expect(e.whiteBalance.temperature == 6400 && !e.whiteBalance.asShot)
            #expect(e.exposure == own.exposure && e.curve == own.curve && e.profile == .vivid, "untouched groups are kept")
        }
    }

    @Test func editedIsRecomputedOnEveryCopy() async {
        let m = await loaded()
        m.perform(.copyAdjustments)
        #expect(m.pasteOptions.groups == [.exposure, .hdr])
        m.focus(m.visibleIDs[1], extend: false)
        m.updateEdit(label: "wb") { $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 4000, tint: 0)) }
        m.perform(.copyAdjustments)
        #expect(m.pasteOptions.groups == [.whiteBalance])
        m.focus(m.visibleIDs[2], extend: false)
        m.perform(.copyAdjustments)
        #expect(m.pasteOptions.groups.isEmpty && m.toast?.message.hasSuffix("has no edits to copy") == true)
        let depth = m.history.undoStack.count
        m.focus(m.visibleIDs[3], extend: false)
        #expect(m.applyToSelection() == 0 && m.toast?.message.hasPrefix("No adjustments to apply") == true)
        #expect(m.history.undoStack.count == depth)
    }

    @Test func customTicksPersistAndChangeWhatApplyPastes() async {
        let m = await loaded()
        m.perform(.copyAdjustments)
        let options = CopyOptions(app: m)
        #expect(options.sourceName == m.focusedPhoto?.filename)
        options.toggle(.hdr) // untick HDR after copying: Custom, from what Edited showed
        #expect(options.mode == .custom && options.groups == [.exposure])
        let reread = AppSettings(defaults: m.settings.userDefaults)
        #expect(reread.copyMode == .custom && reread.copyGroups == [.exposure], "kept at once, no Remember box")
        let target = m.visibleIDs[1]
        m.focus(target, extend: false)
        m.applyToSelection()
        #expect(m.edit(for: target).exposure.ev == 0.5 && m.edit(for: target).hdr.shadows == 0, "HDR was unticked")
        options.toggle(.hdr)
        m.applyToSelection()
        #expect(m.edit(for: target).hdr.shadows == 20, "ticked again: the same clipboard pastes it")
    }

    @Test func modesAllNoneAndRelative() async {
        let m = await loaded()
        let options = CopyOptions(app: m)
        #expect(options.sourceName == m.focusedPhoto?.filename, "before a copy: the focused photo")
        #expect(options.changedGroups == [.exposure, .hdr, .crop, .layers])
        #expect(options.groups == [.exposure, .hdr], "N6: crop and layers only when ticked")
        options.selectAll()
        #expect(options.mode == .custom && options.groups == Set(AdjustGroup.allCases))
        options.selectEdited()
        #expect(options.mode == .edited && options.groups == [.exposure, .hdr])
        options.selectCustom()
        #expect(options.mode == .custom && options.groups == Set(AdjustGroup.allCases), "the picker restores the saved ticks (N7)")
        options.selectEdited()
        options.toggle(.levels)
        #expect(options.mode == .custom && options.groups == [.exposure, .hdr, .levels],
                "a row's tick in Edited starts Custom from what was shown")
        options.selectEdited()
        options.selectCustom()
        #expect(options.groups == [.exposure, .hdr, .levels], "and that set is the saved one from then on")
        options.selectNone()
        #expect(options.groups.isEmpty)
        options.toggleRelative(.levels)
        #expect(options.relative.isEmpty, "levels can't be relative")
        options.toggleRelative(.exposure)
        #expect(options.relative == [.exposure])
        options.toggleRelative(.exposure)
        #expect(options.relative.isEmpty)
        #expect(options.keepStraighten)
        options.toggleKeepStraighten()
        #expect(!m.settings.copyKeepsStraighten)
    }

    @Test func nothingTickedCopiesButExplains() async {
        let m = await loaded()
        CopyOptions(app: m).selectNone()
        m.perform(.copyAdjustments)
        #expect(m.clipboard != nil && m.toast?.message.contains("no adjustments are ticked") == true)
        m.focus(m.visibleIDs[1], extend: false)
        let depth = m.history.undoStack.count
        #expect(m.applyToSelection() == 0)
        #expect(m.history.undoStack.count == depth)
        #expect(m.clipboardSummary.contains("nothing to apply"))
    }

    @Test func clipboardSurvivesFocusChanges() async {
        let m = await loaded()
        let src = m.focusedID!
        m.perform(.copyAdjustments)
        m.updateEdit(label: "later") { $0.exposure.ev = -1 } // the clipboard is a snapshot
        for _ in 0..<5 { m.perform(.move(.right, extend: false)) }
        m.browserMode = .grid
        #expect(m.clipboard?.sourceID == src && m.clipboard?.source.exposure.ev == 0.5)
        #expect(CopyOptions(app: m).source.exposure.ev == 0.5, "the options describe the clipboard once copied")
        #expect(m.canApply)
    }

    @Test func applyToManySelectedPhotosIsOneUndoStep() async {
        let m = await loaded()
        m.perform(.copyAdjustments)
        let ids = Array(m.visibleIDs[1...23])
        m.focus(ids[0], extend: false)
        m.focus(ids.last!, extend: true) // ⇧-click
        #expect(m.selectedIDs.count == 23)
        let depth = m.history.undoStack.count
        m.perform(.applyAdjustments)
        #expect(m.history.undoStack.count == depth + 1)
        #expect(m.toast?.message == "Applied to 23 photos")
        #expect(ids.allSatisfy { m.edit(for: $0).exposure.ev == 0.5 })
        m.undo()
        #expect(ids.allSatisfy { m.edit(for: $0).isIdentity })
        m.selectAll()
        #expect(m.applyTargets.count == 39, "⌘A: every photo but the copied one")
        #expect(m.applyToSelection() == 39)
    }

    /// The reviewer's H2: relative groups and layers must never land on their own source.
    @Test func applyNeverPastesOntoTheCopiedPhoto() async {
        let m = await loaded()
        let src = m.focusedID!
        let options = CopyOptions(app: m)
        options.selectEdited()
        options.toggle(.layers)
        options.toggleRelative(.exposure)
        m.perform(.copyAdjustments)
        #expect(!m.canApply, "only the copied photo is selected")
        #expect(m.clipboardSummary.hasPrefix("Select the photos to apply to"))
        #expect(m.applyToSelection() == 0 && m.toast?.message.contains("never pasted onto itself") == true)
        m.selectAll()
        #expect(m.canApply)
        m.perform(.applyAdjustments)
        #expect(m.edit(for: src).exposure.ev == 0.5 && m.edit(for: src).layers.count == 2, "the source is untouched")
        let other = m.edit(for: m.visibleIDs[1])
        #expect(other.exposure.ev == 0.5 && other.layers.count == 2)
    }

    @Test func applyIsOffWithoutClipboardOrSelection() async {
        let m = await loaded()
        #expect(!m.canApply && !CommandCatalog.isEnabled(.applyAdjustments, model: m))
        #expect(m.applyToSelection() == 0 && m.toast?.message == "Copy adjustments first (⇧⌘C)")
        #expect(m.clipboardSummary == "Copy adjustments first (⇧⌘C)")
        m.perform(.copyAdjustments)
        m.focus(m.visibleIDs[1], extend: false)
        #expect(m.canApply && CommandCatalog.isEnabled(.applyAdjustments, model: m))
        m.selection = []
        m.focus(nil, extend: false)
        #expect(m.selectedIDs.isEmpty && !m.canApply)
    }

    @Test func relativeExposureAddsTheChange() async {
        let m = await loaded()
        m.settings.copyRelativeGroups = [.exposure]
        m.perform(.copyAdjustments)
        let ids = Array(m.visibleIDs[1...2])
        m.updateEdits([ids[0]], label: "own") { _, e in e.exposure.ev = 1 }
        m.selection = Set(ids)
        #expect(m.applyToSelection() == 2)
        #expect(m.edit(for: ids[0]).exposure.ev == 1.5 && m.edit(for: ids[1]).exposure.ev == 0.5)
        #expect(m.pasteOptions.isRelative(.exposure) && !m.pasteOptions.isRelative(.hdr))
        #expect(m.clipboardSummary.contains("Exposure (relative)") && m.clipboardSummary.contains("Apply to 2 photos"))
    }

    @Test func clipboardLayersCanBeChosen() async {
        let m = await loaded()
        CopyOptions(app: m).toggle(.layers)
        m.perform(.copyAdjustments)
        let options = CopyOptions(app: m)
        #expect(options.layers.count == 2 && options.isLayerOn(0) && options.isLayerOn(1))
        #expect(m.pasteOptions.layerIDs.isEmpty, "all layers = no filter")
        options.toggleLayer(0)
        #expect(m.pasteOptions.layerIDs == [1])
        options.toggleLayer(1)
        #expect(!m.pasteOptions.groups.contains(.layers), "no layers chosen: the group is skipped")
        options.toggleLayer(1)
        let target = m.visibleIDs[1]
        m.focus(target, extend: false)
        m.applyToSelection()
        #expect(m.edit(for: target).layers.map(\.name) == ["Face"])
        m.focus(m.visibleIDs[0], extend: false)
        m.perform(.copyAdjustments)
        #expect(options.isLayerOn(0), "a new copy takes all its layers again")
    }

    /// The reviewer's M4: a panel header's Copy is that group only, for every Apply.
    @Test func aPanelHeaderCopyAppliesOnlyItsGroup() async {
        let m = await loaded()
        let saved = m.settings.copyGroups
        m.copyGroup(.exposure)
        #expect(m.pasteOptions.groups == [.exposure] && m.settings.copyGroups == saved && m.settings.copyMode == .edited)
        #expect(CopyOptions(app: m).onlyGroup == .exposure)
        let target = m.visibleIDs[1]
        m.focus(target, extend: false)
        #expect(m.clipboardSummary.contains("Exposure only"))
        m.perform(.applyAdjustments)
        #expect(m.edit(for: target).exposure.ev == 0.5 && m.edit(for: target).hdr.shadows == 0, "the toolbar Apply too")
        CopyOptions(app: m).selectEdited()
        #expect(m.clipboard?.onlyGroup == nil && m.pasteOptions.groups == [.exposure, .hdr], "changing the options ends it")
        m.copyGroup(.hdr)
        m.perform(.copyAdjustments)
        #expect(m.clipboard?.onlyGroup == nil, "a full Copy ends it")
    }

    /// The reviewer's M2: per-photo groups are named where they are pasted.
    @Test func perPhotoGroupsAreNamedInTheToastAndTheHover() async {
        let m = await loaded()
        let options = CopyOptions(app: m)
        options.toggle(.crop)
        m.perform(.copyAdjustments)
        #expect(m.toast?.message.hasSuffix(", including Crop & Straighten") == true)
        m.focus(m.visibleIDs[1], extend: false)
        #expect(m.clipboardSummary.contains("Includes per-photo groups: Crop & Straighten (each photo keeps its straighten)"))
        options.toggle(.crop)
        #expect(!m.clipboardSummary.contains("per-photo"))
    }

    /// The reviewer's M6: a pasted crop keeps each photo's rotation, and (by default) its straighten.
    @Test func cropPasteKeepsEachPhotosTurnsAndStraighten() async {
        let m = await loaded()
        let target = m.visibleIDs[1]
        m.updateEdits([target], label: "own") { _, e in
            e.crop.quarterTurns = 1
            e.crop.angle = 1.5
        }
        CopyOptions(app: m).toggle(.crop)
        m.perform(.copyAdjustments)
        m.focus(target, extend: false)
        m.applyToSelection()
        var c = m.edit(for: target).crop
        #expect(c.quarterTurns == 1 && c.angle == 1.5 && c.rect != CGRect(x: 0, y: 0, width: 1, height: 1))
        m.settings.copyKeepsStraighten = false
        m.applyToSelection()
        c = m.edit(for: target).crop
        #expect(c.quarterTurns == 1 && c.angle == 2, "the straighten too when asked; the rotation never")
    }

    /// Re-verify N1: a 4:5 crop at full height pasted onto a frame straightened by 3° (its
    /// straighten kept) shrinks about its centre, same aspect, until no corner leaves the
    /// frame: the pipeline uses a user rect as is, in the viewer and in export.
    @Test func aCropKeptAtAnotherAngleIsInscribedInTheFrame() {
        let size = CGSize(width: 6000, height: 4000)
        var src = EditState()
        src.crop.aspect = .r4x5
        src.crop.rect = CGRect(x: (1 - 3200.0 / 6000) / 2, y: 0, width: 3200.0 / 6000, height: 1)
        var target = EditState()
        target.crop.angle = 3
        #expect(!CropPaste.fits(src.crop.rect, angle: 3, in: size), "the reviewer's case: 2081 px > 2000 px")
        var o = PasteOptions(groups: [.crop])
        o.keepStraighten = true
        let c = AdjustmentClipboard(source: src).apply(to: target, options: o, sourceSize: size, targetSize: size).crop
        #expect(c.angle == 3)
        #expect(CropPaste.fits(c.rect, angle: 3, in: size), "\(c.rect)")
        #expect(!CropPaste.fits(c.rect.insetBy(dx: -0.002, dy: -0.002), angle: 3, in: size), "as large as fits")
        #expect(abs(c.rect.width * 6000 / (c.rect.height * 4000) - 0.8) < 1e-5, "same aspect")
        #expect(abs(c.rect.midX - 0.5) < 1e-6 && abs(c.rect.midY - 0.5) < 1e-6, "same centre")
        // Already inside, full frames (the pipeline auto-fits them) and level crops are left alone.
        let small = CropEdit.with(rect: CGRect(x: 0.3, y: 0.3, width: 0.4, height: 0.4), angle: 3)
        #expect(CropPaste.inscribe(small, in: size) == small)
        let full = CropEdit.with(rect: CGRect(x: 0, y: 0, width: 1, height: 1), angle: 3)
        #expect(CropPaste.inscribe(full, in: size) == full)
        let level = CropEdit.with(rect: src.crop.rect, angle: 0)
        #expect(CropPaste.inscribe(level, in: size) == level)
    }

    /// Re-verify N2: the crop follows the target as shown, its own rotation included.
    @Test func aPortraitCropStaysPortraitOnAPhotoTurnedUpright() {
        let size = CGSize(width: 6000, height: 4000)
        var src = CropEdit()
        src.aspect = .r4x5
        src.rect = CGRect(x: (1 - 3200.0 / 6000) / 2, y: 0, width: 3200.0 / 6000, height: 1) // portrait on landscape
        let turned = CropPaste.adapt(src, from: size, to: size, targetTurns: 1)
        #expect(turned.quarterTurns == 1)
        // Shown upright (turned), a deliberate portrait crop is portrait: landscape in oriented pixels.
        let w = turned.rect.width * 6000, h = turned.rect.height * 4000
        #expect(abs(w / h - 1.25) < 1e-3, "\(turned.rect)")
        #expect(CropPaste.adapt(src, from: size, to: size, targetTurns: 0) == CropPaste.adapt(src, from: size, to: size))
        let r = CGRect(x: 0.1, y: 0.2, width: 0.3, height: 0.4)
        func near(_ a: CGRect, _ b: CGRect) -> Bool {
            abs(a.minX - b.minX) < 1e-12 && abs(a.minY - b.minY) < 1e-12 && abs(a.width - b.width) < 1e-12 && abs(a.height - b.height) < 1e-12
        }
        #expect(near(CropPaste.turn(CropPaste.turn(r, by: 1), by: 3), r) && CropPaste.turn(r, by: 4) == r)
        #expect(near(CropPaste.turn(r, by: 1), CGRect(x: 0.4, y: 0.1, width: 0.4, height: 0.3)), "clockwise: (u, v) → (1 − v, u)")
    }

    /// Re-verify N3: the count is what Apply pastes onto, hidden photos left out.
    @Test func theApplyCountMatchesTheTargets() async {
        let m = await loaded()
        m.perform(.copyAdjustments)
        let ids = Array(m.visibleIDs[0..<30])
        for id in ids.prefix(5) {
            m.focus(id, extend: false)
            m.setRating(5)
        }
        m.selection = Set(ids)
        #expect(m.applyTargetCount == 29 && m.applyTargets.count == 29)
        m.filter = PhotoFilter(minRating: 5)
        let shown = Set(m.visibleIDs).intersection(ids).subtracting([m.clipboard!.sourceID!]).count
        #expect(shown < 29 && m.applyTargetCount == shown && m.applyTargets.count == shown, "\(shown) \(m.applyTargetCount)")
        #expect(m.clipboardSummary.hasPrefix("Apply to \(MatchSummary.photos(shown))"))
    }

    /// Re-verify N5: a source with only per-photo edits copies nothing in Edited, says why,
    /// and Apply stays off.
    @Test func perPhotoOnlyEditsAreExplainedAndApplyStaysOff() async {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "crop") { $0.crop.rect = CGRect(x: 0.1, y: 0.1, width: 0.5, height: 0.5) }
        m.perform(.copyAdjustments)
        #expect(m.toast?.message.contains("only per-photo edits (Crop & Straighten)") == true, "\(m.toast?.message ?? "")")
        m.focus(m.visibleIDs[1], extend: false)
        #expect(m.clipboard != nil && !m.canApply, "nothing to paste: off")
        CopyOptions(app: m).toggle(.crop)
        #expect(m.canApply, "ticked: it pastes the crop")
    }

    /// The reviewer's M1: the clipboard's ids belong to its session.
    @Test func closingTheSessionClearsTheClipboard() async {
        let m = await loaded()
        m.perform(.copyAdjustments)
        await m.closeSession()
        #expect(m.clipboard == nil && m.clipboardLayerIDs.isEmpty && !m.canApply)
    }

    @Test func openingAnotherSessionClearsTheClipboard() async {
        let m = await loaded()
        m.perform(.copyAdjustments)
        let same = m.currentSession!
        await m.open(same)
        #expect(m.clipboard != nil, "reopening the same session keeps it")
        await m.open(m.sessionList.first { $0.id != same.id }!)
        #expect(m.clipboard == nil, "another session's photo #1 is another photo")
    }

    @Test func copyIsOffOnAVideo() async {
        let m = makeModel(store: VideoStore())
        await m.start()
        #expect(await waitUntil { m.photos.count == 2 })
        m.focus(m.photos.first { $0.isVideo }!.id, extend: false)
        #expect(!CommandCatalog.isEnabled(.copyAdjustments, model: m))
        m.copyWithOptions()
        #expect(m.clipboard == nil)
        m.focus(m.photos.first { !$0.isVideo }!.id, extend: false)
        #expect(CommandCatalog.isEnabled(.copyAdjustments, model: m))
    }

    /// The reviewer's L7: without as-shot values a relative white balance is pasted as is: say so.
    @Test func relativeWhiteBalanceWithoutAsShotSaysItWasAbsolute() async {
        let m = await loaded()
        m.updateEdit(label: "wb") { $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 6000, tint: 0)) }
        m.settings.copyRelativeGroups = [.whiteBalance]
        m.perform(.copyAdjustments)
        m.focus(m.visibleIDs[1], extend: false)
        m.applyToSelection()
        #expect(m.toast?.message == "Applied to 1 photo; white balance pasted as is on 1 photo (as-shot unknown)")
    }
}

/// Settings from the removed Copy/Apply sheet carry over (the reviewer's M3).
extension CropEdit {
    static func with(rect: CGRect, angle: Double) -> CropEdit {
        var c = CropEdit()
        c.rect = rect
        c.angle = angle
        return c
    }
}

@MainActor
@Suite struct CopySettingsMigrationTests {
    func defaults(_ body: (UserDefaults) -> Void) -> (AppSettings, UserDefaults) {
        let suite = testSuiteName()
        let d = UserDefaults(suiteName: suite)!
        d.removePersistentDomain(forName: suite)
        body(d)
        return (AppSettings(defaults: d), d)
    }

    @Test func freshPrefsCopyTheEditedGroups() {
        let (s, _) = defaults { _ in }
        #expect(s.copyMode == .edited && s.copyRelativeGroups.isEmpty && s.copyKeepsStraighten)
        #expect(s.copyGroups == AdjustGroup.copiedByDefault)
    }

    @Test func aRememberOffUserKeepsTheirLastRelativeChoices() {
        let (s, d) = defaults {
            $0.set(["groups": ["exposure"], "relative": ["exposure", "whiteBalance"], "layers": [Int]()], forKey: "lastPasteOptions.v2")
            $0.set(["groups": ["hdr"], "relative": [String](), "layers": [Int]()], forKey: "lastPasteOptions")
        }
        #expect(s.copyMode == .edited, "the sheet pre-ticked the edited groups")
        #expect(s.copyRelativeGroups == [.exposure, .whiteBalance])
        #expect(d.object(forKey: "lastPasteOptions.v2") == nil && d.object(forKey: "lastPasteOptions") == nil, "orphans removed")
        #expect(AppSettings(defaults: d).copyRelativeGroups == [.exposure, .whiteBalance], "and kept in the new prefs")
    }

    @Test func aRememberedSelectionBecomesTheCustomTicks() {
        let (s, d) = defaults {
            $0.set(["exposure", "hdr"], forKey: "rememberedCopyGroups.v2")
            $0.set(["exposure"], forKey: "rememberedRelativeGroups")
            $0.set(["groups": ["levels"], "relative": ["hdr"], "layers": [Int]()], forKey: "lastPasteOptions.v2")
        }
        #expect(s.copyMode == .custom && s.copyGroups == [.exposure, .hdr] && s.copyRelativeGroups == [.exposure])
        #expect(d.object(forKey: "lastPasteOptions.v2") == nil)
        s.copyMode = .edited
        #expect(AppSettings(defaults: d).copyMode == .edited, "the mode is kept once chosen")
    }
}

/// A relative white balance onto photos never visited loads their as-shot values first
/// (the reviewer's H3), with real tools.
@MainActor
@Suite struct CopyApplyAsShotTests {
    func app(delay: Duration = .milliseconds(5)) async -> (AppModel, Rnd9AsShotTools, [PhotoID]) {
        let m = makeModel(counts: [6])
        let tools = Rnd9AsShotTools()
        tools.delay = delay
        m.tools = tools
        await m.start()
        let ids = Array(m.visibleIDs.prefix(3))
        tools.asShot[ids[0]] = WhiteBalanceValue(kelvin: 5000, tint: 0)
        tools.asShot[ids[1]] = WhiteBalanceValue(kelvin: 4000, tint: 0)
        tools.asShot[ids[2]] = WhiteBalanceValue(kelvin: 6000, tint: 0)
        m.focus(ids[0], extend: false)
        m.updateEdit(label: "wb") { $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 5500, tint: 0)) }
        m.settings.copyRelativeGroups = [.whiteBalance]
        return (m, tools, ids)
    }

    /// Re-verify N6: no as-shot load (a RAW decode) unless a relative white balance needs it.
    @Test func copyLoadsNothingWithoutRelativeWhiteBalance() async {
        let (m, tools, ids) = await app()
        m.settings.copyRelativeGroups = [.exposure]
        let asked = tools.asked.count
        m.perform(.copyAdjustments)
        try? await Task.sleep(for: .milliseconds(60))
        #expect(tools.asked.count == asked && m.clipboard?.sourceAsShot == nil)
        m.settings.copyRelativeGroups = [.whiteBalance]
        m.perform(.copyAdjustments)
        #expect(await waitUntil { tools.asked.contains(ids[0]) })
    }

    @Test func copyLoadsTheSourcesAsShot() async {
        let (m, _, ids) = await app()
        m.perform(.copyAdjustments)
        #expect(await waitUntil { m.clipboard?.sourceAsShot == WhiteBalanceValue(kelvin: 5000, tint: 0) })
        #expect(m.asShot(for: ids[0]) != nil)
    }

    @Test func aSecondPressWhileLoadingIsIgnored() async {
        let (m, _, ids) = await app(delay: .milliseconds(30))
        m.perform(.copyAdjustments)
        m.selection = Set(ids.dropFirst())
        let depth = m.history.undoStack.count
        #expect(m.applyToSelection() == 0)
        #expect(m.toast?.message.hasPrefix("Loading the as-shot white balance") == true, "said at once")
        #expect(!m.canApply && m.clipboardSummary.hasPrefix("Loading"))
        let pending = m.pendingPaste
        #expect(m.applyToSelection() == 0 && m.toast?.message.hasPrefix("Still loading") == true)
        #expect(m.pendingPaste == pending, "no second paste started")
        #expect(await pending?.value == 2)
        #expect(m.history.undoStack.count == depth + 1, "pasted once")
        #expect(m.pendingPaste == nil && m.canApply)
        // ×1.1 once: 4000 → 4400, 6000 → 6600 (not ×1.21).
        #expect(abs(m.edit(for: ids[1]).whiteBalance.temperature - 4400) < 1)
        #expect(abs(m.edit(for: ids[2]).whiteBalance.temperature - 6600) < 1)
    }

    @Test func closingTheSessionDropsAWaitingPaste() async {
        let (m, _, ids) = await app(delay: .milliseconds(200))
        m.perform(.copyAdjustments)
        m.selection = Set(ids.dropFirst())
        m.applyToSelection()
        let pending = m.pendingPaste
        #expect(pending != nil)
        await m.closeSession()
        #expect(await pending?.value == 0, "nothing pasted after the close")
        #expect(m.edits.isEmpty && m.developInfo.isEmpty, "nothing of the old session written back")
    }

    @Test func thePressesClipboardIsPastedEvenIfCopiedAgainMeanwhile() async {
        let (m, _, ids) = await app(delay: .milliseconds(30))
        m.perform(.copyAdjustments)
        m.selection = [ids[1]]
        m.applyToSelection()
        m.focus(ids[2], extend: false)
        m.updateEdit(label: "other") { $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 9000, tint: 0)) }
        m.perform(.copyAdjustments) // a new clipboard while the first Apply waits
        #expect(await m.pendingPaste?.value == 1)
        #expect(abs(m.edit(for: ids[1]).whiteBalance.temperature - 4400) < 1, "the first clipboard, with its own as-shot")
    }
}

/// Before/after: the grid opens the viewer, an unedited photo says so, and the "before"
/// follows the photo's framing.
@MainActor
@Suite struct BeforeAfterTests {
    @Test func gridOpensTheViewerWithTheSplitOn() async {
        let m = makeModel()
        await m.start()
        m.browserMode = .grid
        m.perform(.toggleBeforeAfter)
        #expect(m.browserMode == .filmstrip && m.viewer.state.beforeAfter)
        m.browserMode = .grid
        m.perform(.toggleBeforeAfter)
        #expect(m.browserMode == .filmstrip && m.viewer.state.beforeAfter, "from the grid it never turns it off unseen")
        m.perform(.toggleBeforeAfter)
        #expect(!m.viewer.state.beforeAfter, "in the viewer, Y toggles")
    }

    /// The reported path: Y pressed while the grid has the keyboard.
    @Test func yInTheGridReachesTheViewer() async {
        let m = makeModel()
        await m.start()
        m.browserMode = .grid
        #expect(m.handleKey(KeyInput("y"), focus: .none, sheetOpen: false, isMainWindow: true))
        #expect(m.browserMode == .filmstrip && m.viewer.state.beforeAfter)
        m.browserMode = .grid
        m.viewer.toggleBeforeAfter()
        m.selection = []
        m.focus(nil, extend: false)
        #expect(m.handleKey(KeyInput("y"), focus: .none, sheetOpen: false, isMainWindow: true))
        #expect(m.browserMode == .grid && !m.viewer.state.beforeAfter, "no photo: no empty viewer")
    }

    @Test func anUneditedPhotoSaysSo() async {
        let m = makeModel()
        await m.start()
        #expect(m.viewer.beforeAfterNote == nil, "split off")
        m.perform(.toggleBeforeAfter)
        #expect(m.viewer.beforeAfterNote == "No edits on this photo: before and after are the same")
        m.updateEdit(label: "crop") { $0.crop.angle = 3 }
        #expect(m.viewer.beforeAfterNote != nil, "framing only: the before keeps it")
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.4 }
        #expect(m.viewer.beforeAfterNote == nil)
        m.perform(.move(.right, extend: false))
        #expect(m.viewer.beforeAfterNote != nil, "the next photo is unedited")
        var styled = m.focusedEdit
        styled.exposure.ev = 1
        m.viewer.setPreviewEdit(styled)
        #expect(m.viewer.beforeAfterNote == nil, "a style hover preview is an edit")
    }

    @Test func framingOnlyEditsMatch() {
        var e = EditState()
        e.crop.rect = CGRect(x: 0.2, y: 0, width: 0.6, height: 1)
        e.lens.distortion = 0.5
        e.personBoxes = [0: CGRect(x: 0, y: 0, width: 0.1, height: 0.1)]
        #expect(ViewerModel.beforeMatchesAfter(e, developed: true), "the before keeps crop and lens")
        #expect(!ViewerModel.beforeMatchesAfter(e, developed: false), "oriented: the before has no lens correction")
        e.lens = LensEdit()
        #expect(ViewerModel.beforeMatchesAfter(e, developed: false))
        e.vignette.amountEV = -1
        #expect(!ViewerModel.beforeMatchesAfter(e, developed: true))
    }

    @Test func draggingTheDividerMovesTheDrawnLine() async {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.5 }
        m.perform(.toggleBeforeAfter)
        #expect(await waitUntil { m.viewer.drawPlan(loupeAt: nil).quads.first?.before != nil })
        let v = m.viewer
        let image = v.layout.main!.image
        let line = v.layout.main!.splitX!
        #expect(abs(line - image.midX) < 0.5, "starts in the middle")
        #expect(v.grabsSplit(atX: line + 10) && !v.grabsSplit(atX: line + 40), "the line and its handle, not the photo")
        let version = v.displayVersion
        v.dragSplit(toX: image.minX + image.width * 0.25)
        #expect(abs(v.state.split - 0.25) < 0.001)
        #expect(v.displayVersion > version, "the Metal surface redraws: the drawn line follows the handle")
        let ndc = Float((image.minX + image.width * 0.25) / 1000 * 2 - 1)
        #expect(abs((v.drawPlan(loupeAt: nil).quads.first?.split ?? 9) - ndc) < 0.001)
        v.dragSplit(toX: -500)
        #expect(v.state.split == 0, "clamped to the image's left edge")
        v.dragSplit(toX: 5000)
        #expect(v.state.split == 1)
        v.dragSplit(toX: image.minX + image.width * 0.3)
        m.perform(.move(.right, extend: false))
        #expect(abs(v.state.split - 0.3) < 0.001 && v.state.beforeAfter, "kept across photos")
        m.perform(.toggleBeforeAfter)
        #expect(!v.grabsSplit(atX: line), "no divider when the split is off")
    }

    @Test func aNewCropReRendersTheBeforeWithoutBlanking() async {
        let r = Vw8GeometryRenderer()
        let m = makeViewerApp(renderer: r)
        m.viewer.settleDelay = .milliseconds(30)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.5 }
        m.perform(.toggleBeforeAfter)
        #expect(await waitUntil { m.viewer.beforeFrame != nil && m.viewer.isRenderIdle })
        #expect(m.viewer.drawPlan(loupeAt: nil).quads.first?.before != nil)
        func aspect(_ f: RenderedFrame?) -> Double { f.map { Double($0.width) / Double($0.height) } ?? 0 }
        m.updateEdit(label: "crop") { $0.crop.rect = CGRect(x: 0, y: 0, width: 0.5, height: 1) }
        #expect(m.viewer.drawPlan(loupeAt: nil).quads.first?.before != nil,
                "until the new frame lands, the split shows the frame on screen and its matching before")
        #expect(await waitUntil { m.viewer.isRenderIdle && m.viewer.renderedQuality == .preview
                && abs(aspect(m.viewer.beforeFrame) - aspect(m.viewer.frame)) < 0.02 })
        #expect(m.viewer.drawPlan(loupeAt: nil).quads.first?.before != nil)
    }

    /// The reviewer's M5: a framing drag renders one frame per tick, the before once at settle.
    @Test func aFramingDragRendersTheBeforeOnceAtTheEnd() async {
        let r = Vw8GeometryRenderer()
        let m = makeViewerApp(renderer: r)
        m.viewer.settleDelay = .milliseconds(30)
        m.viewer.gestureSettleFallback = .seconds(30)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.5 }
        m.perform(.toggleBeforeAfter)
        #expect(await waitUntil { m.viewer.beforeFrame != nil && m.viewer.isRenderIdle && m.viewer.renderedQuality == .preview })
        let n = r.requests.count
        m.viewer.beginEditGesture()
        for i in 1...3 {
            m.updateEdit(label: "lens") { $0.lens.keystoneVertical = Double(i) }
            #expect(await waitUntil { m.viewer.isRenderIdle })
        }
        #expect(r.requests.count - n <= 3, "drafts only: \(r.requests.count - n) renders")
        m.endEditGesture()
        #expect(await waitUntil { m.viewer.renderedQuality == .preview && m.viewer.isRenderIdle
                && m.viewer.drawPlan(loupeAt: nil).quads.first?.before != nil })
    }
}
