import CoreGraphics
import Foundation

/// The eyedropper loupe's work in flight (not observed).
@MainActor
final class LoupeJobs {
    /// What a region render was asked for: a new photo or edit makes it stale.
    struct RegionKey: Equatable {
        var photo: PhotoID
        var edit: EditState
        var format: FramePixelFormat
        var space: CGSize
    }

    /// One white-balance pick: the same key always gives the same value.
    struct PickKey: Equatable {
        var photo: PhotoID
        var edit: EditState
        /// Normalized in the final frame (a pixel's centre in developed display).
        var point: CGPoint
        var sample: Int
    }

    var region: Task<Void, Never>?
    var regionKey: RegionKey?
    /// The cursor moved while a region rendered: render again for where it is now.
    var regionPending = false
    var pick: Task<Void, Never>?
    var pickKey: PickKey?
    /// A pick waits for the cursor to rest this long (hovering doesn't start one per pixel).
    var pickDelay: Duration = .milliseconds(100)
    var pickDebounce: Task<Void, Never>?
    /// Recent picks (newest last), so a click applies the value already shown.
    var picks: [(key: PickKey, result: Result<WhiteBalanceEdit, LoupeError>)] = []

    func result(for key: PickKey) -> Result<WhiteBalanceEdit, LoupeError>? {
        picks.last { $0.key == key }?.result
    }

    func remember(_ key: PickKey, _ result: Result<WhiteBalanceEdit, LoupeError>) {
        picks.removeAll { $0.key == key }
        picks.append((key, result))
        if picks.count > 16 { picks.removeFirst(picks.count - 16) }
    }
}

/// Why a pick gave no value, in words.
struct LoupeError: Error, Equatable {
    let message: String
}

/// Where the cursor is, in the space the loupe samples.
struct LoupeTarget: Equatable {
    var pixel: LoupePixel
    /// The frame space in pixels (the final frame at 1:1, or the oriented image).
    var space: CGSize
    var developed: Bool
    /// The pick point in the final frame (nil: not known yet in oriented display).
    var finalPoint: CGPoint?
}

extension ViewerModel {
    /// Which loupe the active tool uses (nil: none).
    var loupeKind: PickerLoupe.Kind? {
        switch activeTool {
        case .wbPicker: return .whiteBalance
        case .colorPicker:
            switch app?.colorTools.mode {
            case .pickRange?, .pickMaskRange?: return .colorRange
            case .pickSkin?: return .skinTone
            default: return nil
            }
        default: return nil
        }
    }

    /// The loupe's frames: 8-bit like an SDR view (P3 on wide-gamut screens), so the
    /// readout and the clipping test are in display terms, also on EDR screens.
    var loupeFormat: FramePixelFormat {
        displayWideGamut && capabilities.rendersGeometry ? .rgba8DisplayP3 : .rgba8Srgb
    }

    /// The pixel under the display-normalized point `u`: in the final frame at 1:1 in
    /// developed display (crop, turns and straighten included), else the oriented image.
    func loupeTarget(display u: CGPoint) -> LoupeTarget? {
        guard Self.inUnit(u) else { return nil }
        let developed = mapping != nil
        let space = mapping?.nativeSize ?? orientedImageSize
        guard space.width >= 1, space.height >= 1 else { return nil }
        let f = developed ? u : oriented(u)
        guard Self.inUnit(f) else { return nil }
        let px = LoupePixel(x: min(max(Int(f.x * space.width), 0), Int(space.width) - 1),
                            y: min(max(Int(f.y * space.height), 0), Int(space.height) - 1))
        let centre = CGPoint(x: (Double(px.x) + 0.5) / space.width, y: (Double(px.y) + 0.5) / space.height)
        return LoupeTarget(pixel: px, space: space, developed: developed,
                           finalPoint: developed ? centre : finalFramePoint(forDisplay: u))
    }

    /// The edit the loupe shows and a pick starts from: the photo's stored edit (what a
    /// click changes), also in before/after mode and while a style is previewed, so the
    /// loupe's pixels, its readout and the click always agree.
    func loupeEdit(for id: PhotoID) -> EditState { app?.edit(for: id) ?? EditState() }

    /// The cursor moved over the viewer (view points) or left it: show, move or hide the
    /// loupe. Cheap on the main thread: a region render starts only when the cursor
    /// leaves the one on hand (one in flight; the newest position renders next). Not shown
    /// until the developed frame's mapping exists (the pixel under the cursor isn't known
    /// before: an oriented-space loupe would jump when it lands).
    func updatePickerLoupe(at p: CGPoint?) {
        let loupe = pickerLoupe
        guard let p, let kind = loupeKind, let app, let photo = app.focusedPhoto, !photo.isVideo,
              !app.isOriginalMissing(photo.id), !developedGeometry || mapping != nil,
              let pane = layout.main, let u = layout.imagePoint(at: p), let target = loupeTarget(display: u) else {
            loupe.hide()
            return
        }
        if loupe.sample != app.settings.pickerSample { loupe.sample = app.settings.pickerSample }
        loupe.show(kind, cursor: p, bounds: pane.frame, pixel: target.pixel)
        requestLoupeRegion(for: photo, target: target)
        refreshLoupeReadout(target: target)
    }

    /// The picker's sample size changed (the panel's menu).
    public func setPickerSample(_ sample: PickerSample) {
        app?.settings.pickerSample = sample
        pickerLoupeChanged()
    }

    /// The tool, the photo or its edit changed: refresh the loupe where the cursor is.
    func pickerLoupeChanged() {
        updatePickerLoupe(at: cursor)
    }

    private func requestLoupeRegion(for photo: Photo, target: LoupeTarget) {
        let loupe = pickerLoupe, jobs = loupeJobs
        let key = LoupeJobs.RegionKey(photo: photo.id, edit: loupeEdit(for: photo.id), format: loupeFormat,
                                      space: target.space)
        let radius = loupe.visibleRadius + loupe.sample.pixels / 2 + 1
        if let r = loupe.region, r.photo == key.photo, r.edit == key.edit, r.frame.format == key.format,
           r.space == key.space, r.covers(target.pixel, radius: radius) { return }
        if let inFlight = jobs.regionKey {
            if inFlight == key {
                jobs.regionPending = true
                return
            }
            jobs.region?.cancel() // a stale photo or edit
        }
        let side = max(64, 2 * radius + 32)
        let (w, h) = (Int(target.space.width), Int(target.space.height))
        let (sw, sh) = (min(side, w), min(side, h))
        let x0 = min(max(target.pixel.x - sw / 2, 0), w - sw), y0 = min(max(target.pixel.y - sh / 2, 0), h - sh)
        let rect = CGRect(x: Double(x0) / Double(w), y: Double(y0) / Double(h), width: Double(sw) / Double(w),
                          height: Double(sh) / Double(h))
        let request = RenderRequest(maxPixelSize: max(w, h), scale: 1, region: rect, format: key.format,
                                    quality: capabilities.rendersGeometry ? .full : .preview, priority: .interactive,
                                    geometry: target.developed ? .final : .oriented)
        jobs.regionKey = key
        jobs.regionPending = false
        let renderer = renderer
        jobs.region = Task { [weak self] in
            let frame = try? await renderer.render(photo, edit: key.edit, request: request)
            guard let self, !Task.isCancelled, self.loupeJobs.regionKey == key else { return }
            self.loupeJobs.regionKey = nil
            if let frame {
                self.pickerLoupe.region = PickerLoupe.Region(frame: frame, rect: frame.region ?? CGRect(x: 0, y: 0, width: 1, height: 1),
                                                             space: key.space, photo: key.photo, edit: key.edit)
            }
            if self.loupeJobs.regionPending || frame != nil { self.pickerLoupeChanged() }
        }
    }

    private func refreshLoupeReadout(target: LoupeTarget) {
        let loupe = pickerLoupe
        guard let kind = loupe.kind, let app, let id = app.focusedID, let region = loupe.region, region.photo == id,
              region.space == target.space,
              let m = PickerLoupe.measure(region, at: target.pixel, sample: loupe.sample.pixels) else {
            loupe.readout = nil
            return
        }
        let rgb = SIMD3((0..<3).map { Int((min(max(m.rgb[$0], 0), 1) * 255).rounded()) })
        var readout = PickerLoupe.Readout(rgb: SIMD3(rgb[0], rgb[1], rgb[2]), clipped: m.clipped,
                                          neutrality: PickerLoupe.neutrality(m.rgb))
        var settled = loupeJobs.regionKey == nil
        if kind == .whiteBalance {
            if let key = pickKey(id, target: target) {
                switch loupeJobs.result(for: key) ?? localPick(key, pixel: target.pixel) {
                case .success(let wb)?: readout.whiteBalance = wb
                case .failure(let e)?: readout.whiteBalanceError = e.message
                case nil:
                    settled = false
                    schedulePick(key)
                }
            } else {
                settled = false
            }
        }
        if loupe.readout != readout { loupe.readout = readout }
        if settled { loupe.announce(PickerLoupe.spoken(readout, kind: kind)) }
    }

    /// The pick for the cursor's pixel with the photo's stored edit (what a click changes).
    /// Through the pipeline it needs the final-frame point; without it (demo) the loupe's
    /// own pixel identifies it.
    private func pickKey(_ id: PhotoID, target: LoupeTarget) -> LoupeJobs.PickKey? {
        guard let app else { return nil }
        let own = CGPoint(x: (Double(target.pixel.x) + 0.5) / target.space.width,
                          y: (Double(target.pixel.y) + 0.5) / target.space.height)
        guard let point = app.tools == nil ? own : target.finalPoint else { return nil }
        return LoupeJobs.PickKey(photo: id, edit: loupeEdit(for: id), point: point, sample: pickerLoupe.sample.pixels)
    }

    /// Without the pipeline's tools (demo): neutralize the loupe's own pixels at `pixel`,
    /// when they show this edit.
    private func localPick(_ key: LoupeJobs.PickKey, pixel: LoupePixel) -> Result<WhiteBalanceEdit, LoupeError>? {
        guard app?.tools == nil, let region = pickerLoupe.region, region.photo == key.photo, region.edit == key.edit,
              let m = PickerLoupe.measure(region, at: pixel, sample: key.sample) else { return nil }
        let wb = WhiteBalancePicker.neutralize(key.edit.whiteBalance, sampledRGB: (m.rgb.x, m.rgb.y, m.rgb.z))
        loupeJobs.remember(key, .success(wb))
        return .success(wb)
    }

    /// Start a pick once the cursor rests (`pickDelay`); moving on cancels it.
    private func schedulePick(_ key: LoupeJobs.PickKey) {
        let jobs = loupeJobs
        jobs.pickDebounce?.cancel()
        let delay = jobs.pickDelay
        jobs.pickDebounce = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            self?.startPick(key)
        }
    }

    /// Compute a pick through the pipeline (one in flight; the newest key runs next).
    private func startPick(_ key: LoupeJobs.PickKey) {
        let jobs = loupeJobs
        if let k = jobs.pickKey {
            if k == key { return }
            if k.photo == key.photo && k.edit == key.edit { return } // finishes, then the newest runs
            jobs.pick?.cancel()
        }
        guard let app, let photo = app.photo(key.photo) else { return }
        jobs.pickKey = key
        jobs.pick = Task { [weak self] in
            let result: Result<WhiteBalanceEdit, LoupeError>
            do {
                result = .success(try await app.pickedWhiteBalance(photo, edit: key.edit, at: key.point, sample: key.sample))
            } catch is CancellationError {
                return
            } catch {
                result = .failure(LoupeError(message: Self.pickFailure(error)))
            }
            guard let self, self.loupeJobs.pickKey == key else { return }
            self.loupeJobs.pickKey = nil
            self.loupeJobs.remember(key, result)
            self.pickerLoupeChanged()
        }
    }

    nonisolated static func pickFailure(_ error: Error) -> String {
        error.localizedDescription.contains("black or clipped") ? "Clipped or black: pick another neutral" : error.localizedDescription
    }

    /// Mean sRGB of the loupe's sample at the display point `u` (the colour pickers use
    /// it so a click takes what the loupe shows); nil when the loupe has no region there.
    func loupeSample(display u: CGPoint) -> SIMD3<Double>? {
        guard let app, let target = loupeTarget(display: u), let region = pickerLoupe.region,
              region.photo == app.focusedID, region.space == target.space,
              region.edit == loupeEdit(for: region.photo) else { return nil }
        return PickerLoupe.measure(region, at: target.pixel, sample: pickerLoupe.sample.pixels)?.rgb
    }

    /// WB picker click at a **display** point: apply what the loupe reads there, on the
    /// focused photo only. The value is the loupe's own (computed by the same call with
    /// the same inputs when it isn't ready yet). The picker stays on for more tries.
    public func pickWhiteBalance(at u: CGPoint) {
        guard let app, let id = app.focusedID else { return }
        if let target = loupeTarget(display: u), let key = pickKey(id, target: target) {
            if let known = loupeJobs.result(for: key) ?? localPick(key, pixel: target.pixel) {
                applyPick(known, to: id)
                return
            }
            if app.tools != nil, let photo = app.photo(id) {
                Task { [weak self] in
                    do {
                        let wb = try await app.pickedWhiteBalance(photo, edit: key.edit, at: key.point, sample: key.sample)
                        self?.loupeJobs.remember(key, .success(wb))
                        self?.applyPick(.success(wb), to: id)
                    } catch is CancellationError {
                    } catch {
                        self?.applyPick(.failure(LoupeError(message: Self.pickFailure(error))), to: id)
                    }
                }
                return
            }
        }
        // No region yet (demo renderers): the develop frame on screen, never a preview.
        let at = frame?.mapping != nil ? u : oriented(u)
        guard let rgb = sampleFrame(at: at) else { return }
        app.updateEdits([id], label: "White Balance Picker") { _, e in
            e.whiteBalance = WhiteBalancePicker.neutralize(e.whiteBalance, sampledRGB: rgb)
        }
    }

    private func applyPick(_ result: Result<WhiteBalanceEdit, LoupeError>, to id: PhotoID) {
        guard let app, app.focusedID == id else { return }
        switch result {
        case .success(let wb): app.applyPickedWhiteBalance(wb, to: id)
        case .failure(let e): app.show(Toast(e.message, symbol: "eyedropper", isError: true))
        }
    }
}
