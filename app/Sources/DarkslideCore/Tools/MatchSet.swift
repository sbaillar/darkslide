import DarkslideFFI
import Foundation

/// A photo and its edit, as sent to a match.
public struct MatchItem: Equatable, Sendable {
    public var photo: Photo
    public var edit: EditState
    public init(photo: Photo, edit: EditState) {
        self.photo = photo
        self.edit = edit
    }
}

/// One target's exposure match (ds-ffi `ExposureMatchResult`).
public struct ExposureMatchOutcome: Equatable, Sendable {
    public enum Source: Equatable, Sendable { case metered, exif, unmatched }
    /// Stops to add to the target's exposure (keeps it within −5…5).
    public var deltaEV: Double
    public var source: Source
    /// Metering and EXIF disagree by more than 3 stops (most likely another scene).
    public var exifDisagrees = false
    public var error: String?

    public init(deltaEV: Double, source: Source, exifDisagrees: Bool = false, error: String? = nil) {
        self.deltaEV = deltaEV
        self.source = source
        self.exifDisagrees = exifDisagrees
        self.error = error
    }
}

/// One target's white-balance match: the Kelvin/tint to set (`nil`: it already
/// matches, or failed with `error`).
public struct WhiteBalanceMatchOutcome: Equatable, Sendable {
    public var value: WhiteBalanceValue?
    public var error: String?
    public init(value: WhiteBalanceValue?, error: String? = nil) {
        self.value = value
        self.error = error
    }
}

/// Matching a set to its reference (live mode: `RustDevelopTools`, ds-ffi `matching.rs`).
/// Results come back one per target, in order.
public protocol SetMatching: Sendable {
    /// Exposure deltas that match each target's metered mid-tones to the reference's
    /// (EXIF when metering is unreliable).
    func matchExposures(reference: MatchItem, targets: [MatchItem]) async throws -> [ExposureMatchOutcome]
    /// The reference's Kelvin/tint for each target (As Shot resolved on both sides).
    func matchWhiteBalance(reference: MatchItem, targets: [MatchItem]) async throws -> [WhiteBalanceMatchOutcome]
}

extension RustDevelopTools: SetMatching {
    static func matchPhoto(_ item: MatchItem) throws -> MatchPhoto {
        MatchPhoto(path: try file(item.photo).path, adjustmentsJson: try json(item.edit))
    }

    public func matchExposures(reference: MatchItem, targets: [MatchItem]) async throws -> [ExposureMatchOutcome] {
        let r = try Self.matchPhoto(reference)
        let t = try targets.map(Self.matchPhoto)
        return try await Self.run { try await DarkslideFFI.matchExposures(reference: r, targets: t, token: $0) }.map {
            let source: ExposureMatchOutcome.Source = switch $0.source {
            case .metered: .metered
            case .exif: .exif
            case .unmatched: .unmatched
            }
            return ExposureMatchOutcome(deltaEV: Double($0.deltaEv), source: source, exifDisagrees: $0.exifDisagrees,
                                        error: $0.error)
        }
    }

    public func matchWhiteBalance(reference: MatchItem, targets: [MatchItem]) async throws -> [WhiteBalanceMatchOutcome] {
        let r = try Self.matchPhoto(reference)
        let t = try targets.map(Self.matchPhoto)
        return try await Self.run { try await DarkslideFFI.matchWhiteBalance(reference: r, targets: t, token: $0) }.map {
            WhiteBalanceMatchOutcome(value: $0.whiteBalance.map(Self.value), error: $0.error)
        }
    }
}

/// Toast texts of the match commands (pure, tested).
public enum MatchSummary {
    static func photos(_ n: Int) -> String { n == 1 ? "1 photo" : "\(n) photos" }

    public static func exposure(_ out: [ExposureMatchOutcome]) -> String {
        let matched = out.filter { $0.source != .unmatched }
        let exif = matched.count(where: { $0.source == .exif })
        let failed = out.count - matched.count
        var s = matched.isEmpty ? "No exposures could be matched" : "Matched the exposure of \(photos(matched.count))"
        if exif > 0 { s += " (\(exif) from camera settings)" }
        if failed > 0, !matched.isEmpty { s += "; \(photos(failed)) could not be measured" }
        let disagree = matched.count(where: { $0.source == .metered && $0.exifDisagrees })
        if disagree > 0 {
            s += "; camera settings disagreed by more than 3 EV for \(photos(disagree)) (another scene?)"
        }
        return s
    }

    public static func whiteBalance(_ out: [WhiteBalanceMatchOutcome]) -> String {
        let changed = out.count(where: { $0.value != nil })
        let failed = out.count(where: { $0.error != nil })
        var s = changed == 0 ? "White balance already matches" : "Matched the white balance of \(photos(changed))"
        if failed > 0 { s += "; \(photos(failed)) could not be read" }
        return s
    }
}

/// Models with a match in flight: a second ⌥⇧⌘M while one runs would add its deltas
/// twice. Observable, so menu enablement refreshes when a match starts and ends.
@MainActor @Observable final class MatchesInFlight {
    static let shared = MatchesInFlight()
    var models: Set<ObjectIdentifier> = []
}

/// Edit ▸ Match Total Exposures (⌥⇧⌘M) / Match White Balance to Focused Photo: the
/// focused photo is the reference, the other selected photos the targets; each runs as
/// one undo step. Metering renders have no AI masks (AI-masked layers meter as if their
/// AI shape were empty).
extension AppModel {
    /// The tools that can match a set (live mode only).
    var setMatcher: (any SetMatching)? { tools as? any SetMatching }

    /// The other selected photos a match changes (videos aren't editable).
    public var matchTargets: [PhotoID] {
        guard let f = focusedID, photo(f)?.isVideo == false else { return [] }
        return selectedIDs.filter { $0 != f && photo($0)?.isVideo == false }
    }

    /// A match is running (the commands are off until it lands).
    public var isMatchingSet: Bool { MatchesInFlight.shared.models.contains(ObjectIdentifier(self)) }

    /// Whether the match commands can run (a reference, a target, the tools, and no
    /// match already running).
    public var canMatchSet: Bool { setMatcher != nil && !isMatchingSet && !matchTargets.isEmpty }

    /// The reference and targets as they are now.
    private func matchItems() -> (MatchItem, [MatchItem])? {
        guard !isMatchingSet, let f = focusedID, let ref = photo(f) else { return nil }
        let targets = matchTargets.compactMap { id in photo(id).map { MatchItem(photo: $0, edit: edit(for: id)) } }
        return targets.isEmpty ? nil : (MatchItem(photo: ref, edit: edit(for: f)), targets)
    }

    /// Match Total Exposures: each target's exposure changes by its metered delta (added
    /// to whatever it is when the match finishes), all in one undo step.
    @discardableResult
    public func matchTotalExposures() -> Task<Void, Never>? {
        guard let matcher = setMatcher, let items = matchItems() else { return nil }
        let (ref, targets) = items
        let me = ObjectIdentifier(self)
        MatchesInFlight.shared.models.insert(me)
        return sessionBound { [self] isCurrent in
            defer { MatchesInFlight.shared.models.remove(me) }
            do {
                let out = try await matcher.matchExposures(reference: ref, targets: targets)
                // M1: the results belong to the session the match started in.
                guard isCurrent() else { return }
                var deltas: [PhotoID: Double] = [:]
                for (t, o) in zip(targets, out) where o.source != .unmatched && photo(t.photo.id) != nil {
                    deltas[t.photo.id] = o.deltaEV
                }
                updateEdits(Array(deltas.keys), label: "Match Total Exposures") { id, e in
                    let ev = e.exposure.ev + (deltas[id] ?? 0)
                    e.exposure.ev = min(5, max(-5, (ev * 100).rounded() / 100))
                }
                show(Toast(MatchSummary.exposure(out), symbol: "plusminus.circle"))
            } catch is CancellationError {
            } catch {
                if isCurrent() { show(error) } // M1: not over another session
            }
        }
    }

    /// Match White Balance to Focused Photo: each target gets the reference's Kelvin/tint
    /// (As Shot resolved on both sides), in one undo step.
    @discardableResult
    public func matchWhiteBalanceToFocused() -> Task<Void, Never>? {
        guard let matcher = setMatcher, let items = matchItems() else { return nil }
        let (ref, targets) = items
        let me = ObjectIdentifier(self)
        MatchesInFlight.shared.models.insert(me)
        return sessionBound { [self] isCurrent in
            defer { MatchesInFlight.shared.models.remove(me) }
            do {
                let out = try await matcher.matchWhiteBalance(reference: ref, targets: targets)
                guard isCurrent() else { return } // M1
                var values: [PhotoID: WhiteBalanceValue] = [:]
                for (t, o) in zip(targets, out) where photo(t.photo.id) != nil {
                    if let v = o.value { values[t.photo.id] = v }
                }
                updateEdits(Array(values.keys), label: "Match White Balance") { id, e in
                    if let v = values[id] { e.whiteBalance = .custom(v) }
                }
                show(Toast(MatchSummary.whiteBalance(out), symbol: "thermometer.medium"))
            } catch is CancellationError {
            } catch {
                if isCurrent() { show(error) } // M1: not over another session
            }
        }
    }
}
