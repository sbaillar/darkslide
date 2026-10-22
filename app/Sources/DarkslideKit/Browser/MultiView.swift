import CoreGraphics

/// [multiview] Several selected photos in the viewer at once (Capture One's multi-view):
/// which grid shows them largest, where each tile goes, and which tile is under a click.
/// Pure, so the layout is testable; `ViewerArea` draws it.
public enum MultiViewLayout {
    /// More than this many selected: the first ones and a "+N more" tile.
    public static let maxTiles = 12
    /// Gap between tiles and around the grid, in points.
    public static let spacing: CGFloat = 4

    public struct Grid: Equatable, Sendable {
        public var columns: Int
        public var rows: Int
    }

    public enum Content: Hashable, Sendable {
        case photo(PhotoID)
        /// The photos not shown.
        case more(Int)
    }

    public struct Tile: Equatable, Sendable {
        public var content: Content
        public var frame: CGRect
    }

    public struct Plan: Equatable, Sendable {
        public var tiles: [Tile]
        /// Where the live viewer goes: the focused photo's tile, or the whole area.
        public var focusFrame: CGRect
        /// Photos the tiles show (the "+N more" tile not counted).
        public var photoCount: Int { tiles.filter { if case .photo = $0.content { true } else { false } }.count }
        public var isMulti: Bool { tiles.count > 1 }

        /// The tile under a view point.
        public func tile(at point: CGPoint) -> Tile? { tiles.first { $0.frame.contains(point) } }

        /// 1-based position of `id` among the shown photos.
        public func position(of id: PhotoID) -> Int? {
            var n = 0
            for t in tiles {
                guard case .photo(let p) = t.content else { continue }
                n += 1
                if p == id { return n }
            }
            return nil
        }
    }

    /// The photos to show (at most `maxTiles` tiles) and how many are left over.
    /// The focused photo is always among them.
    public static func shown(_ ids: [PhotoID], focused: PhotoID?) -> (ids: [PhotoID], more: Int) {
        guard ids.count > maxTiles else { return (ids, 0) }
        var head = Array(ids.prefix(maxTiles - 1))
        if let f = focused, ids.contains(f), !head.contains(f) { head[head.count - 1] = f }
        return (head, ids.count - head.count)
    }

    /// The columns × rows that make the photos (aspect w/h each) largest in `size`:
    /// the most image area in total, then the fewest empty cells, then the wider grid.
    public static func grid(aspects: [Double], in size: CGSize, spacing: CGFloat = spacing) -> Grid {
        let n = aspects.count
        guard n > 1, size.width > 0, size.height > 0 else { return Grid(columns: 1, rows: 1) }
        var best = Grid(columns: 1, rows: n)
        var bestScore = (area: -1.0, empty: Int.max)
        for cols in 1...n {
            let rows = (n + cols - 1) / cols
            let cell = cellSize(Grid(columns: cols, rows: rows), in: size, spacing: spacing)
            guard cell.width > 0, cell.height > 0 else { continue }
            let area = aspects.reduce(0.0) { sum, a in
                let r = ThumbnailLayout.fit(aspect: a, in: cell)
                return sum + Double(r.width * r.height)
            }
            let empty = cols * rows - n
            // Within half a percent counts as a tie (floating point; near-equal layouts).
            let better = area > bestScore.area * 1.005
                || (area >= bestScore.area * 0.995 && empty < bestScore.empty)
            if better {
                best = Grid(columns: cols, rows: rows)
                bestScore = (area, empty)
            }
        }
        return best
    }

    static func cellSize(_ g: Grid, in size: CGSize, spacing: CGFloat) -> CGSize {
        CGSize(width: (size.width - spacing * CGFloat(g.columns + 1)) / CGFloat(g.columns),
               height: (size.height - spacing * CGFloat(g.rows + 1)) / CGFloat(g.rows))
    }

    /// Tile frames, row by row; an incomplete last row is centered.
    public static func frames(count: Int, grid g: Grid, in size: CGSize, spacing: CGFloat = spacing) -> [CGRect] {
        let cell = cellSize(g, in: size, spacing: spacing)
        guard count > 0, cell.width > 0, cell.height > 0 else { return [] }
        return (0..<count).map { i in
            let row = i / g.columns, col = i % g.columns
            let inRow = min(g.columns, count - row * g.columns)
            let rowWidth = CGFloat(inRow) * cell.width + CGFloat(inRow - 1) * spacing
            let x0 = (size.width - rowWidth) / 2
            return CGRect(x: x0 + CGFloat(col) * (cell.width + spacing),
                          y: spacing + CGFloat(row) * (cell.height + spacing),
                          width: cell.width, height: cell.height)
        }
    }

    /// The layout for `ids` (the selection in browser order) in a viewer of `size`.
    /// Fewer than two photos, or no focus among them: the single view.
    public static func plan(ids: [PhotoID], focused: PhotoID?, aspect: (PhotoID) -> Double,
                            in size: CGSize, spacing: CGFloat = spacing) -> Plan {
        let whole = CGRect(origin: .zero, size: size)
        guard ids.count > 1, let focused, ids.contains(focused) else {
            return Plan(tiles: focused.map { [Tile(content: .photo($0), frame: whole)] } ?? [], focusFrame: whole)
        }
        let (show, more) = shown(ids, focused: focused)
        var contents = show.map(Content.photo)
        var aspects = show.map(aspect)
        if more > 0 {
            contents.append(.more(more))
            aspects.append(1.5)
        }
        let g = grid(aspects: aspects, in: size, spacing: spacing)
        let rects = frames(count: contents.count, grid: g, in: size, spacing: spacing)
        guard rects.count == contents.count else {
            return Plan(tiles: [Tile(content: .photo(focused), frame: whole)], focusFrame: whole)
        }
        let tiles = zip(contents, rects).map { Tile(content: $0, frame: $1) }
        let focus = tiles.first { $0.content == .photo(focused) }?.frame ?? whole
        return Plan(tiles: tiles, focusFrame: focus)
    }

    /// VoiceOver label for a tile: "Photo 2 of 4, focused".
    public static func accessibilityLabel(position: Int, of count: Int, focused: Bool) -> String {
        "Photo \(position) of \(count)\(focused ? ", focused" : "")"
    }

    /// The overflow tile's text.
    public static func moreLabel(_ n: Int) -> String { "+\(n) more" }
}
