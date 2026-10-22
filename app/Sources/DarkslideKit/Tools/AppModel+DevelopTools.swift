import CoreGraphics
import Foundation

/// App-level develop tools on the pipeline (live mode). Every tool degrades to the
/// preview-based behaviour when `tools` is nil (demo mode) or fails.
extension AppModel {
    /// As-shot white balance of `id`, when known (see `loadDevelopInfo`).
    public func asShot(for id: PhotoID) -> WhiteBalanceValue? { developInfo[id]?.asShot }

    /// Fill `developInfo` for `ids` (as-shot WB, lens profile, ISO). Cheap after the first
    /// call per file (the tools cache the decoded RAW and the facts).
    public func loadDevelopInfo(for ids: [PhotoID]) async {
        guard let tools else { return }
        let session = currentSession?.id
        for id in ids where developInfo[id] == nil {
            if Task.isCancelled || currentSession?.id != session { return } // the session closed: its ids mean nothing now
            guard let p = photo(id), !p.isVideo else { continue }
            if let info = try? await tools.info(for: p), !Task.isCancelled, currentSession?.id == session {
                developInfo[id] = info
            }
        }
    }

    /// Auto white balance and auto tone from the pipeline on top of `base` (the
    /// preview-based estimate from the AI service keeps its straighten).
    func developAuto(_ p: Photo, base: EditState, estimate: EditState) async throws -> EditState {
        guard let tools, !p.isVideo else { return estimate }
        let wb = try await tools.autoWhiteBalance(for: p)
        var e = estimate
        e.whiteBalance = .custom(wb)
        // Tone is measured with the new white balance and the rest of the edit.
        return try await tools.autoTone(for: p, edit: e)
    }

    /// WB picker: neutralize the `sample`×`sample` pixels at `finalPoint` (normalized in
    /// the final, cropped and turned frame of the develop render) on photo `id`.
    @discardableResult
    public func pickWhiteBalance(id: PhotoID, finalPoint: CGPoint,
                                 sample: Int = PickerSample.box5.pixels) -> Task<Void, Never>? {
        guard tools != nil, let p = photo(id), !p.isVideo else { return nil }
        let edit = edit(for: id)
        return sessionBound { [self] isCurrent in
            do {
                let wb = try await pickedWhiteBalance(p, edit: edit, at: finalPoint, sample: sample)
                guard isCurrent(), photo(id) != nil else { return }
                applyPickedWhiteBalance(wb, to: id)
            } catch is CancellationError {
            } catch {
                if isCurrent() { show(error) }
            }
        }
    }

    /// What a WB pick gives (the loupe shows it before the click applies it): the
    /// pipeline's value, rounded as the sliders store it.
    func pickedWhiteBalance(_ p: Photo, edit: EditState, at finalPoint: CGPoint, sample: Int) async throws -> WhiteBalanceEdit {
        guard let tools else { throw DevelopToolsError.unavailable("no develop pipeline") }
        return .custom(try await tools.pickWhiteBalance(for: p, edit: edit, at: finalPoint, sample: sample))
    }

    func applyPickedWhiteBalance(_ wb: WhiteBalanceEdit, to id: PhotoID) {
        updateEdits([id], label: "White Balance Picker") { _, e in e.whiteBalance = wb }
    }
}
