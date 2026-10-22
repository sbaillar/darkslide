import Foundation

/// [multiview] The selection in the viewer, and the resizable filmstrip.
extension AppModel {
    /// The selected photos the viewer shows side by side, in browser order; empty for
    /// the single view (one photo, Show Selection in Viewer off, the grid, or Compare,
    /// which pins its own pair).
    public var multiViewIDs: [PhotoID] {
        guard settings.showSelectionInViewer, browserMode == .filmstrip, selection.count > 1,
              viewer.state.comparePinned == nil, let f = focusedID, selection.contains(f) else { return [] }
        return selectedIDs
    }

    /// ↑/↓: a grid row, a row of the filmstrip when it has more than one per row (multi-row
    /// at the bottom; at the side ↑/↓ run along the column), else the previous/next photo.
    var verticalStep: Int {
        switch browserMode {
        case .grid: return max(1, gridColumns)
        case .filmstrip:
            let gridded = filmstrip.position.isVertical || filmstrip.lines > 1
            let shown = filmstrip.lines > 0 && !filmstripHidden && !panelsHidden && !viewer.state.fullScreen
            return gridded && shown ? max(1, filmstripColumns) : 1
        case .map, .library: return 1
        }
    }

    /// The separator was dragged to `lines` (live while dragging; `ended` remembers a multi-line size).
    public func resizeFilmstrip(to lines: Int, ended: Bool) {
        filmstrip = ended ? filmstrip.dragged(to: lines)
            : FilmstripLayout(position: filmstrip.position, lines: lines, last: filmstrip.last)
    }

    /// Double-click on the separator: one line ↔ the last size.
    public func toggleFilmstripLines() { filmstrip = filmstrip.toggled }

    /// View ▸ Filmstrip ▸ Bottom / Left / Right.
    public func placeFilmstrip(_ position: FilmstripPosition) { filmstrip = filmstrip.moved(to: position) }
}
