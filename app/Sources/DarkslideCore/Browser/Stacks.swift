import Foundation
import Observation

/// A burst: consecutive frames from one camera shot less than `maxGap` apart.
public struct PhotoStack: Identifiable, Hashable, Sendable {
    /// Stable id: the first member (capture order), independent of which frame is on top.
    public let id: PhotoID
    public let memberIDs: [PhotoID]
    /// The frame shown when collapsed.
    public let topID: PhotoID

    public var count: Int { memberIDs.count }
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
    /// stacks; members keep `photos` order; the top follows `StackBuilder.top`,
    /// preferring a frame in `best` (e.g. ds-ai's pick from the last AI Cull).
    public func update(photos: [Photo], grouping: any StackGrouping, best: Set<PhotoID> = []) {
        let order = Dictionary(photos.enumerated().map { ($1.id, $0) }, uniquingKeysWith: { a, _ in a })
        stacks = grouping.groups(photos).compactMap { ids -> PhotoStack? in
            let members = ids.filter { order[$0] != nil }.sorted { order[$0]! < order[$1]! }
            guard members.count >= 2 else { return nil }
            let membersPhotos = members.map { photos[order[$0]!] }
            let hinted = membersPhotos.first { best.contains($0.id) && $0.pick != .rejected }
            return PhotoStack(id: members[0], memberIDs: members, topID: (hinted ?? StackBuilder.top(of: membersPhotos)).id)
        }
        .sorted { order[$0.id]! < order[$1.id]! }
        var index: [PhotoID: Int] = [:]
        for (i, s) in stacks.enumerated() { for m in s.memberIDs { index[m] = i } }
        stackIndexByMember = index
        revision += 1
        collapsed = collapsed.filter { id in stacks.contains { $0.id == id } }
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

    private func currentTop(of s: PhotoStack, lookup: ((PhotoID) -> Photo?)?) -> PhotoID {
        guard let lookup else { return s.topID }
        let members = s.memberIDs.compactMap(lookup)
        return members.isEmpty ? s.topID : StackBuilder.top(of: members).id
    }

    /// The current top of every collapsed stack, in stack order.
    private func collapsedTops(lookup: ((PhotoID) -> Photo?)?) -> [PhotoID] {
        stacks.filter { collapsed.contains($0.id) }.map { currentTop(of: $0, lookup: lookup) }
    }
}
