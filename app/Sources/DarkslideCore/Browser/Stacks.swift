import Foundation
import Observation

/// A burst: consecutive frames from one camera shot less than `maxGap` apart.
public struct PhotoStack: Identifiable, Hashable, Sendable {
    /// Stable id: the first member (capture order), independent of which frame is on top.
    public let id: PhotoID
    public let memberIDs: [PhotoID]
    /// The frame shown when collapsed, as `CoverRule` decided when the stack was built
    /// (`StackState.currentTop` re-decides from current ratings).
    public let topID: PhotoID
    /// The frame the user chose (Use as Cover), when it is one of the members. Kept even
    /// while a higher-rated frame outranks it: it is the cover again when it ties.
    public let coverID: PhotoID?

    public init(id: PhotoID, memberIDs: [PhotoID], topID: PhotoID, coverID: PhotoID? = nil) {
        self.id = id
        self.memberIDs = memberIDs
        self.topID = topID
        self.coverID = coverID.flatMap { memberIDs.contains($0) ? $0 : nil }
    }

    public var count: Int { memberIDs.count }
}

/// What the AI knows about frames (the last AI Cull): its picks, and its quality scores.
public struct CoverAI: Equatable, Sendable {
    public var best: Set<PhotoID>
    public var scores: [PhotoID: Double]

    public init(best: Set<PhotoID> = [], scores: [PhotoID: Double] = [:]) {
        self.best = best
        self.scores = scores
    }

    /// Its preference for `id`: picked first, then the score; nil when it has no opinion.
    func rank(_ id: PhotoID) -> (Int, Double)? {
        guard best.contains(id) || scores[id] != nil else { return nil }
        return (best.contains(id) ? 1 : 0, scores[id] ?? 0)
    }
}

/// Why a stack shows the frame it shows.
public enum CoverSource: Equatable, Sendable {
    /// The user chose it (Use as Cover) among the top-rated frames.
    case user
    /// The AI preferred it among the top-rated frames.
    case ai
    /// The only top-rated frame, or the usual rule among them.
    case rule
}

public struct CoverDecision: Equatable, Sendable {
    public var cover: PhotoID
    public var source: CoverSource
    /// The frames that can be the cover: the highest-rated (rejects only when all are).
    public var candidates: [PhotoID]
    /// Their rating.
    public var topRating: Int
    /// More than one frame shares the top rating (the user or the AI breaks the tie).
    public var isTie: Bool { candidates.count > 1 }
}

/// Which frame of a burst is its cover. The highest star rating wins (rejected frames
/// only when every frame is rejected). Among frames tied for it: the user's choice, else
/// the AI's preference (its pick, then its score), else picked before unflagged, earliest
/// first (`StackBuilder.top`). Pure.
public enum CoverRule {
    /// The frames that can be the cover, in `members` order, and their rating.
    public static func candidates(_ members: [Photo]) -> (ids: [PhotoID], topRating: Int) {
        let kept = members.filter { $0.pick != .rejected }
        let pool = kept.isEmpty ? members : kept
        let top = pool.map(\.rating).max() ?? 0
        return (pool.filter { $0.rating == top }.map(\.id), top)
    }

    /// `members` must not be empty.
    public static func decide(_ members: [Photo], chosen: PhotoID?, ai: CoverAI = CoverAI()) -> CoverDecision {
        let (ids, top) = candidates(members)
        if let c = chosen, ids.contains(c) { return CoverDecision(cover: c, source: .user, candidates: ids, topRating: top) }
        if ids.count > 1 {
            var pick: (id: PhotoID, rank: (Int, Double))?
            for id in ids {
                guard let r = ai.rank(id) else { continue }
                if pick == nil || r > pick!.rank { pick = (id, r) }
            }
            if let pick { return CoverDecision(cover: pick.id, source: .ai, candidates: ids, topRating: top) }
        }
        let set = Set(ids)
        let cover = StackBuilder.top(of: members.filter { set.contains($0.id) }).id
        return CoverDecision(cover: cover, source: .rule, candidates: ids, topRating: top)
    }

    /// The burst strip's order: highest rating first, rejects last, ties in capture order.
    public static func stripOrder(_ members: [Photo]) -> [PhotoID] {
        members.enumerated().sorted { a, b in
            let ka = (a.element.pick == .rejected ? 1 : 0, -a.element.rating, a.offset)
            let kb = (b.element.pick == .rejected ? 1 : 0, -b.element.rating, b.offset)
            return ka < kb
        }.map(\.element.id)
    }
}

/// Pure stack building (bursts by camera + capture-time gap).
public enum StackBuilder {
    /// Group `photos` (in display order) into stacks of ≥ 2 frames. Two neighbours
    /// join when they share a camera and were captured ≤ `maxGap` seconds apart
    /// (either direction, so a reversed sort still stacks).
    public static func build(_ photos: [Photo], maxGap: TimeInterval = 1.0) -> [PhotoStack] {
        var stacks: [PhotoStack] = []
        var run: [Photo] = []
        func flush() {
            if run.count >= 2 {
                stacks.append(PhotoStack(id: run[0].id, memberIDs: run.map(\.id), topID: top(of: run).id))
            }
            run.removeAll(keepingCapacity: true)
        }
        for p in photos {
            if let last = run.last,
               last.meta.camera == p.meta.camera,
               abs(p.meta.captureDate.timeIntervalSince(last.meta.captureDate)) <= maxGap {
                run.append(p)
            } else {
                flush()
                run.append(p)
            }
        }
        flush()
        return stacks
    }

    /// Best frame: picked beats unflagged beats rejected, then higher rating, then earliest.
    public static func top(of members: [Photo]) -> Photo {
        func rank(_ p: Photo) -> Int {
            let pick = switch p.pick {
            case .picked: 2
            case .none: 1
            case .rejected: 0
            }
            return pick * 10 + p.rating
        }
        var best = members[0]
        for p in members.dropFirst() where rank(p) > rank(best) { best = p }
        return best
    }
}

/// Splits photos into burst groups (every photo in exactly one group, in capture order).
public protocol StackGrouping: Sendable {
    func groups(_ photos: [Photo]) -> [[PhotoID]]
}

/// Demo/fallback grouping: same camera, ≤ `maxGap` s apart (`StackBuilder`).
public struct CaptureTimeGrouping: StackGrouping {
    public var maxGap: TimeInterval
    public init(maxGap: TimeInterval = 1.0) { self.maxGap = maxGap }

    public func groups(_ photos: [Photo]) -> [[PhotoID]] {
        let stacks = StackBuilder.build(photos, maxGap: maxGap)
        let stacked = Set(stacks.flatMap(\.memberIDs))
        var out: [[PhotoID]] = stacks.map(\.memberIDs)
        out += photos.filter { !stacked.contains($0.id) }.map { [$0.id] }
        return out
    }
}

/// Which stacks are collapsed, and the resulting display order.
@MainActor @Observable
public final class StackState {
    public private(set) var stacks: [PhotoStack] = []
    /// Collapsed stack ids.
    public private(set) var collapsed: Set<PhotoID> = []
    @ObservationIgnored private var stackIndexByMember: [PhotoID: Int] = [:]
    /// Bumped by every rebuild; part of the display cache's identity.
    @ObservationIgnored private var revision = 0
    @ObservationIgnored private var display: DisplayCache?
    /// Full recomputations of `displayIDs` (tests assert that unchanged input is served from the cache).
    @ObservationIgnored public private(set) var displayBuilds = 0
    /// The AI's picks and scores the covers were decided with.
    @ObservationIgnored public private(set) var ai = CoverAI()
    /// The stacked photos as of the last rebuild (re-deciding covers without one).
    @ObservationIgnored private var members: [PhotoID: Photo] = [:]

    /// What `displayIDs` last computed, and what it depended on.
    private struct DisplayCache {
        var visible: [PhotoID]
        var collapsed: Set<PhotoID>
        var revision: Int
        /// The top frame of each collapsed stack when `lookup` was last consulted.
        var tops: [PhotoID]
        var result: [PhotoID]
    }

    public init() {}

    /// Rebuild from the current photos, keeping collapse state for stacks that still exist.
    public func update(photos: [Photo], maxGap: TimeInterval = 1.0) {
        update(photos: photos, grouping: CaptureTimeGrouping(maxGap: maxGap))
    }

    /// Rebuild from `grouping` (ds-ai bursts in live mode). Groups of one are not
    /// stacks; members keep `photos` order; the top follows `CoverRule`: the highest
    /// rating, then the user's chosen cover (one of `covers` still in the stack), then the
    /// AI's preference (`best`, e.g. ds-ai's picks from the last AI Cull, and `scores`).
    public func update(photos: [Photo], grouping: any StackGrouping, best: Set<PhotoID> = [], scores: [PhotoID: Double] = [:],
                       covers: [StackCover] = []) {
        ai = CoverAI(best: best, scores: scores)
        let order = Dictionary(photos.enumerated().map { ($1.id, $0) }, uniquingKeysWith: { a, _ in a })
        var snapshot: [PhotoID: Photo] = [:]
        stacks = grouping.groups(photos).compactMap { ids -> PhotoStack? in
            let members = ids.filter { order[$0] != nil }.sorted { order[$0]! < order[$1]! }
            guard members.count >= 2 else { return nil }
            let membersPhotos = members.map { photos[order[$0]!] }
            for p in membersPhotos { snapshot[p.id] = p }
            let chosen = StackCovers.cover(for: members, in: covers)
            return PhotoStack(id: members[0], memberIDs: members,
                              topID: CoverRule.decide(membersPhotos, chosen: chosen, ai: ai).cover, coverID: chosen)
        }
        .sorted { order[$0.id]! < order[$1.id]! }
        members = snapshot
        var index: [PhotoID: Int] = [:]
        for (i, s) in stacks.enumerated() { for m in s.memberIDs { index[m] = i } }
        stackIndexByMember = index
        revision += 1
        collapsed = collapsed.filter { id in stacks.contains { $0.id == id } }
    }

    /// Apply changed cover choices to the current stacks (no regrouping; collapse state stays).
    public func apply(covers: [StackCover]) {
        let updated = stacks.map { s in
            let chosen = StackCovers.cover(for: s.memberIDs, in: covers)
            let photos = s.memberIDs.compactMap { members[$0] }
            let top = photos.count == s.count ? CoverRule.decide(photos, chosen: chosen, ai: ai).cover : s.topID
            return PhotoStack(id: s.id, memberIDs: s.memberIDs, topID: top, coverID: chosen)
        }
        guard updated != stacks else { return }
        stacks = updated
        revision += 1
    }

    public func stack(of id: PhotoID) -> PhotoStack? {
        stackIndexByMember[id].map { stacks[$0] }
    }

    public func isCollapsed(_ stack: PhotoStack) -> Bool { collapsed.contains(stack.id) }

    /// Collapse/expand the stack containing `id`. Returns false if `id` isn't stacked.
    @discardableResult
    public func toggleStack(of id: PhotoID) -> Bool {
        guard let s = stack(of: id) else { return false }
        if collapsed.contains(s.id) { collapsed.remove(s.id) } else { collapsed.insert(s.id) }
        return true
    }

    public func collapseAll() { collapsed = Set(stacks.map(\.id)) }
    public func expandAll() { collapsed = [] }

    /// Member count to badge on `id` when it stands for a collapsed stack.
    public func badgeCount(for id: PhotoID) -> Int? {
        guard let s = stack(of: id), collapsed.contains(s.id) else { return nil }
        return s.count
    }

    /// `visibleIDs` with each collapsed stack reduced to one representative: its
    /// top frame if visible, otherwise its first visible member (so filters never
    /// hide a whole burst just because the top frame was filtered out).
    /// Pass `lookup` to pick each collapsed stack's top from current ratings/flags
    /// (cost: members of collapsed stacks only), so rating a frame can promote it
    /// without an O(n) stack rebuild.
    public func displayIDs(from visibleIDs: [PhotoID], lookup: ((PhotoID) -> Photo?)? = nil) -> [PhotoID] {
        guard !collapsed.isEmpty else { return visibleIDs }
        // The tops are re-read on every call (that is what makes a rating observed and
        // promotes a frame); only when nothing they depend on changed is the layout reused.
        let tops = collapsedTops(lookup: lookup)
        if let c = display, c.revision == revision, c.collapsed == collapsed, c.tops == tops, c.visible == visibleIDs {
            return c.result
        }
        displayBuilds += 1
        let visible = Set(visibleIDs)
        var shown: Set<PhotoID> = [] // stack ids already represented
        var out: [PhotoID] = []
        out.reserveCapacity(visibleIDs.count)
        for id in visibleIDs {
            guard let s = stack(of: id), collapsed.contains(s.id) else {
                out.append(id)
                continue
            }
            if shown.contains(s.id) { continue }
            let top = currentTop(of: s, lookup: lookup)
            out.append(visible.contains(top) ? top : id)
            shown.insert(s.id)
        }
        display = DisplayCache(visible: visibleIDs, collapsed: collapsed, revision: revision, tops: tops, result: out)
        return out
    }

    /// The frame a stack shows now (`CoverRule`), re-decided from current ratings and
    /// flags when `lookup` is given.
    public func currentTop(of s: PhotoStack, lookup: ((PhotoID) -> Photo?)?) -> PhotoID {
        guard let lookup else { return s.topID }
        return decision(of: s, lookup: lookup)?.cover ?? s.topID
    }

    /// Why the stack shows its cover, from current ratings (nil: none of its photos found).
    public func decision(of s: PhotoStack, lookup: (PhotoID) -> Photo?) -> CoverDecision? {
        let photos = s.memberIDs.compactMap(lookup)
        return photos.isEmpty ? nil : CoverRule.decide(photos, chosen: s.coverID, ai: ai)
    }

    /// The current top of every collapsed stack, in stack order.
    private func collapsedTops(lookup: ((PhotoID) -> Photo?)?) -> [PhotoID] {
        stacks.filter { collapsed.contains($0.id) }.map { currentTop(of: $0, lookup: lookup) }
    }
}
