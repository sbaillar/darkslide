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
/// size a double-click on its separator goes back to.
public struct FilmstripLayout: Equatable, Codable, Sendable {
    public var position: FilmstripPosition
    public var lines: Int
    /// The last size above one line.
    public var last: Int

    public init(position: FilmstripPosition = .bottom, lines: Int = 1, last: Int = 3) {
        self.position = position
        self.lines = max(lines, 0)
        self.last = max(last, 2)
    }

    /// Saved values are clamped like new ones.
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(position: (try? c.decode(FilmstripPosition.self, forKey: .position)) ?? .bottom,
                  lines: try c.decode(Int.self, forKey: .lines), last: try c.decode(Int.self, forKey: .last))
    }

    /// A drag ended at `lines`: a multi-line size is remembered for the double-click.
    public func dragged(to lines: Int) -> FilmstripLayout {
        FilmstripLayout(position: position, lines: lines, last: lines > 1 ? lines : last)
    }

    /// Double-click on the separator: one line ↔ the last size.
    public var toggled: FilmstripLayout {
        lines == 1 ? FilmstripLayout(position: position, lines: last, last: last)
            : FilmstripLayout(position: position, lines: 1, last: lines > 1 ? lines : last)
    }

    /// Moved to `position`; the size goes with it (lines mean columns at the side).
    public func moved(to position: FilmstripPosition) -> FilmstripLayout {
        FilmstripLayout(position: position, lines: lines, last: last)
    }
}

/// [multiview] Filmstrip sizes, snapped to whole lines, along its position's axis:
/// heights at the bottom, widths at the side. Pure; `MainWindow` lays it out.
public enum FilmstripSizing {
    /// The compact filter bar above the thumbnails (and its divider).
    public static let barHeight: CGFloat = 40
    /// One row as the strip always had it: the cell plus its padding.
    public static let rowHeight: CGFloat = 112
    public static let padding: CGFloat = 4
    public static let spacing: CGFloat = 4
    /// Side padding of the thumbnails.
    public static let sidePadding: CGFloat = 8
    /// The viewer never gets shorter (bottom) or narrower (side) than this from a larger strip.
    public static let minViewerHeight: CGFloat = 240
    public static let minViewerWidth: CGFloat = 320

    public static var cellHeight: CGFloat { rowHeight - 2 * padding }
    public static var cellWidth: CGFloat { ThumbnailLayout.filmstripCellWidth(height: cellHeight) }

    /// One more line: a row's height, or a column's width.
    static func step(_ p: FilmstripPosition) -> CGFloat { (p.isVertical ? cellWidth : cellHeight) + spacing }
    static func minViewer(_ p: FilmstripPosition) -> CGFloat { p.isVertical ? minViewerWidth : minViewerHeight }

    /// The strip's size along its axis for `lines` (0: collapsed, nothing but the separator).
    public static func extent(lines: Int, at p: FilmstripPosition) -> CGFloat {
        guard lines > 0 else { return 0 }
        let first = p.isVertical ? 2 * sidePadding + cellWidth : barHeight + rowHeight
        return first + CGFloat(lines - 1) * step(p)
    }

    /// The most lines that leave the viewer its minimum of `available` (viewer + strip
    /// along the axis); at least 1.
    public static func maxLines(available: CGFloat, at p: FilmstripPosition) -> Int {
        max(1, Int(((available - minViewer(p) - extent(lines: 1, at: p)) / step(p)).rounded(.down)) + 1)
    }

    /// The lines shown for a wanted count in `available` points.
    public static func clamped(_ lines: Int, available: CGFloat, at p: FilmstripPosition) -> Int {
        lines <= 0 ? 0 : min(lines, maxLines(available: available, at: p))
    }

    /// The whole lines nearest a strip dragged to `size`; below half a line it collapses.
    public static func lines(forExtent size: CGFloat, available: CGFloat, at p: FilmstripPosition) -> Int {
        let one = extent(lines: 1, at: p)
        guard size >= one / 2 else { return 0 }
        let n = Int(((size - one) / step(p)).rounded()) + 1
        return clamped(max(n, 1), available: available, at: p)
    }

    /// Thumbnails per row of the strip: from the width at the bottom, its lines at the side.
    public static func columns(width: CGFloat) -> Int {
        max(1, Int(((width - 2 * sidePadding + spacing) / (cellWidth + spacing)).rounded(.down)))
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
