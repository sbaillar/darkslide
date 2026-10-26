import CoreGraphics

/// [multiview] Where the filmstrip sits: under the viewer, or a column beside it.
public enum FilmstripPosition: String, CaseIterable, Codable, Sendable {
    case bottom, left, right

    /// A column beside the viewer (its lines are columns, dragged sideways).
    public var isVertical: Bool { self != .bottom }
    public var title: String {
        switch self {
        case .bottom: "Bottom"
        case .left: "Left"
        case .right: "Right"
        }
    }
}

/// [multiview] The filmstrip's layout: where it is and how many lines of thumbnails it
/// shows (rows at the bottom, columns at the side; 0: collapsed), plus the multi-line
/// size a double-click on its separator goes back to, and how large its thumbnails are.
public struct FilmstripLayout: Equatable, Codable, Sendable {
    public var position: FilmstripPosition
    public var lines: Int
    /// The last size above one line.
    public var last: Int
    /// The thumbnails' cell height in points (`FilmstripSizing.cellHeights`).
    public var cell: CGFloat

    public init(position: FilmstripPosition = .bottom, lines: Int = 1, last: Int = 3,
                cell: CGFloat = FilmstripSizing.defaultCellHeight) {
        self.position = position
        self.lines = max(lines, 0)
        self.last = max(last, 2)
        self.cell = min(max(cell.rounded(), FilmstripSizing.cellHeights.lowerBound), FilmstripSizing.cellHeights.upperBound)
    }

    /// Saved values are clamped like new ones; a layout saved before the thumbnail size
    /// could change keeps the size it had.
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(position: (try? c.decode(FilmstripPosition.self, forKey: .position)) ?? .bottom,
                  lines: try c.decode(Int.self, forKey: .lines), last: try c.decode(Int.self, forKey: .last),
                  cell: try c.decodeIfPresent(CGFloat.self, forKey: .cell) ?? FilmstripSizing.defaultCellHeight)
    }

    /// A drag ended at `lines`: a multi-line size is remembered for the double-click.
    public func dragged(to lines: Int) -> FilmstripLayout {
        FilmstripLayout(position: position, lines: lines, last: lines > 1 ? lines : last, cell: cell)
    }

    /// Double-click on the separator: one line ↔ the last size.
    public var toggled: FilmstripLayout {
        lines == 1 ? FilmstripLayout(position: position, lines: last, last: last, cell: cell)
            : FilmstripLayout(position: position, lines: 1, last: lines > 1 ? lines : last, cell: cell)
    }

    /// Moved to `position`; the size goes with it (lines mean columns at the side).
    public func moved(to position: FilmstripPosition) -> FilmstripLayout {
        FilmstripLayout(position: position, lines: lines, last: last, cell: cell)
    }

    /// Thumbnails `cell` points high (clamped to the range); the lines stay.
    public func sized(_ cell: CGFloat) -> FilmstripLayout {
        FilmstripLayout(position: position, lines: lines, last: last, cell: cell)
    }

    /// View ▸ Larger / Smaller Filmstrip Thumbnails: `steps` notches of `FilmstripSizing.cellStep`.
    public func stepped(_ steps: Int) -> FilmstripLayout {
        sized(cell + CGFloat(steps) * FilmstripSizing.cellStep)
    }
}

/// [multiview] Filmstrip sizes, snapped to whole lines, along its position's axis:
/// heights at the bottom, widths at the side, for cells `cell` points high. Pure;
/// `MainWindow` lays it out.
public enum FilmstripSizing {
    /// The compact filter bar above the thumbnails (and its divider).
    public static let barHeight: CGFloat = 40
    /// The cell height the strip always had (a 112-point row with its padding), and the
    /// range of the size slider: up to four times as high.
    public static let defaultCellHeight: CGFloat = 104
    public static let cellHeights: ClosedRange<CGFloat> = 104...416
    /// One View-menu notch: half the default (six notches across the range).
    public static let cellStep: CGFloat = 52
    public static let padding: CGFloat = 4
    public static let spacing: CGFloat = 4
    /// Side padding of the thumbnails.
    public static let sidePadding: CGFloat = 8
    /// The viewer never gets shorter (bottom) or narrower (side) than this from a larger strip.
    public static let minViewerHeight: CGFloat = 240
    public static let minViewerWidth: CGFloat = 320

    /// One row: the cell plus its padding.
    public static func rowHeight(_ cell: CGFloat) -> CGFloat { cell + 2 * padding }
    public static func cellWidth(_ cell: CGFloat) -> CGFloat { ThumbnailLayout.filmstripCellWidth(height: cell) }

    /// One more line: a row's height, or a column's width.
    static func step(_ p: FilmstripPosition, _ cell: CGFloat) -> CGFloat { (p.isVertical ? cellWidth(cell) : cell) + spacing }
    static func minViewer(_ p: FilmstripPosition) -> CGFloat { p.isVertical ? minViewerWidth : minViewerHeight }

    /// The strip's size along its axis for `lines` (0: collapsed, nothing but the separator).
    public static func extent(lines: Int, at p: FilmstripPosition, cell: CGFloat) -> CGFloat {
        guard lines > 0 else { return 0 }
        let first = p.isVertical ? 2 * sidePadding + cellWidth(cell) : barHeight + rowHeight(cell)
        return first + CGFloat(lines - 1) * step(p, cell)
    }

    /// The largest cell up to `cell` whose one line still leaves the viewer its minimum of
    /// `available`: large thumbnails shrink in a small window rather than squash the
    /// viewer (never below the default; a tiny window still shows a line).
    public static func fitted(_ cell: CGFloat, available: CGFloat, at p: FilmstripPosition) -> CGFloat {
        let room = available - minViewer(p)
        // A column is the cell × 1.15, rounded: half a point spare keeps it inside.
        let most = p.isVertical ? ((room - 2 * sidePadding - 0.5) / 1.15).rounded(.down) : room - barHeight - 2 * padding
        return max(cellHeights.lowerBound, min(cell, most))
    }

    /// The most lines that leave the viewer its minimum of `available` (viewer + strip
    /// along the axis); at least 1.
    public static func maxLines(available: CGFloat, at p: FilmstripPosition, cell: CGFloat) -> Int {
        max(1, Int(((available - minViewer(p) - extent(lines: 1, at: p, cell: cell)) / step(p, cell)).rounded(.down)) + 1)
    }

    /// The lines shown for a wanted count in `available` points.
    public static func clamped(_ lines: Int, available: CGFloat, at p: FilmstripPosition, cell: CGFloat) -> Int {
        lines <= 0 ? 0 : min(lines, maxLines(available: available, at: p, cell: cell))
    }

    /// The whole lines nearest a strip dragged to `size`; below half a line it collapses.
    public static func lines(forExtent size: CGFloat, available: CGFloat, at p: FilmstripPosition, cell: CGFloat) -> Int {
        let one = extent(lines: 1, at: p, cell: cell)
        guard size >= one / 2 else { return 0 }
        let n = Int(((size - one) / step(p, cell)).rounded()) + 1
        return clamped(max(n, 1), available: available, at: p, cell: cell)
    }

    /// Thumbnails per row of the strip: from the width at the bottom, its lines at the side.
    public static func columns(width: CGFloat, cell: CGFloat) -> Int {
        max(1, Int(((width - 2 * sidePadding + spacing) / (cellWidth(cell) + spacing)).rounded(.down)))
    }
}

/// [multiview] The separator's VoiceOver value.
public enum FilmstripText {
    public static func size(lines: Int, vertical: Bool) -> String {
        guard lines > 0 else { return "Collapsed" }
        let unit = vertical ? "column" : "row"
        return "\(lines) \(unit)\(lines == 1 ? "" : "s")"
    }
}
