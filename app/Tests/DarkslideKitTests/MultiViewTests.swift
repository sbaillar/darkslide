import CoreGraphics
import Foundation
import Testing
@testable import DarkslideKit

/// [multiview] Grid choice, tile frames and hit-testing for several photos in the viewer.
@Suite struct MultiViewLayoutTests {
    let wide = CGSize(width: 1200, height: 700)
    let tall = CGSize(width: 700, height: 1200)
    func ids(_ n: Int) -> [PhotoID] { (1...n).map { PhotoID(Int64($0)) } }
    func grid(_ n: Int, _ size: CGSize, aspect: Double = 1.5) -> MultiViewLayout.Grid {
        MultiViewLayout.grid(aspects: Array(repeating: aspect, count: n), in: size)
    }

    @Test func gridsMatchCaptureOne() {
        #expect(grid(1, wide) == .init(columns: 1, rows: 1))
        #expect(grid(2, wide) == .init(columns: 2, rows: 1), "two: side by side")
        #expect(grid(2, tall) == .init(columns: 1, rows: 2), "two in a tall viewer: stacked")
        #expect(grid(3, CGSize(width: 1800, height: 600)) == .init(columns: 3, rows: 1), "three across when wide")
        #expect(grid(3, wide) == .init(columns: 2, rows: 2), "else 2+1")
        #expect(grid(4, wide) == .init(columns: 2, rows: 2))
        #expect(grid(5, wide) == .init(columns: 3, rows: 2))
        #expect(grid(6, wide) == .init(columns: 3, rows: 2))
    }

    @Test func aspectRatiosChooseTheGrid() {
        // Portraits in a wide viewer: side by side, never stacked.
        #expect(grid(2, wide, aspect: 2.0 / 3.0) == .init(columns: 2, rows: 1))
        // Panoramas in a squarish viewer: stacked.
        #expect(grid(2, CGSize(width: 900, height: 800), aspect: 3) == .init(columns: 1, rows: 2))
        // Four portraits in a wide viewer: one row of four beats 2×2.
        #expect(grid(4, CGSize(width: 1600, height: 600), aspect: 2.0 / 3.0) == .init(columns: 4, rows: 1))
        #expect(MultiViewLayout.grid(aspects: [1.5, 1.5], in: .zero) == .init(columns: 1, rows: 1), "no room: nothing to choose")
    }

    @Test func framesFillAndCenterTheLastRow() {
        let g = MultiViewLayout.Grid(columns: 2, rows: 2)
        let f = MultiViewLayout.frames(count: 3, grid: g, in: CGSize(width: 1000, height: 600), spacing: 10)
        #expect(f.count == 3)
        #expect(f[0] == CGRect(x: 10, y: 10, width: 485, height: 285))
        #expect(f[1] == CGRect(x: 505, y: 10, width: 485, height: 285))
        #expect(f[2].minY == 305 && abs(f[2].midX - 500) < 0.001, "the lone tile of the last row is centered")
        #expect(MultiViewLayout.frames(count: 0, grid: g, in: wide).isEmpty)
        #expect(MultiViewLayout.frames(count: 2, grid: g, in: CGSize(width: 5, height: 5), spacing: 10).isEmpty, "too small")
    }

    @Test func planPutsTheViewerOnTheFocusedTile() {
        let all = ids(4)
        let plan = MultiViewLayout.plan(ids: all, focused: all[2], aspect: { _ in 1.5 }, in: wide)
        #expect(plan.isMulti && plan.tiles.count == 4 && plan.photoCount == 4)
        #expect(plan.focusFrame == plan.tiles[2].frame)
        #expect(plan.position(of: all[2]) == 3 && plan.position(of: PhotoID(99)) == nil)
        // Hit-testing: the tile under a point, nothing in the gaps.
        #expect(plan.tile(at: CGPoint(x: plan.tiles[1].frame.midX, y: plan.tiles[1].frame.midY))?.content == .photo(all[1]))
        #expect(plan.tile(at: CGPoint(x: 1, y: 1)) == nil)
    }

    @Test func planFallsBackToTheSingleView() {
        let whole = CGRect(origin: .zero, size: wide)
        let one = MultiViewLayout.plan(ids: ids(1), focused: PhotoID(1), aspect: { _ in 1.5 }, in: wide)
        #expect(!one.isMulti && one.focusFrame == whole && one.tiles.count == 1)
        let unfocused = MultiViewLayout.plan(ids: ids(3), focused: PhotoID(9), aspect: { _ in 1.5 }, in: wide)
        #expect(!unfocused.isMulti && unfocused.focusFrame == whole, "focus outside the selection: single view")
        let none = MultiViewLayout.plan(ids: [], focused: nil, aspect: { _ in 1.5 }, in: wide)
        #expect(none.tiles.isEmpty && none.focusFrame == whole)
        let tiny = MultiViewLayout.plan(ids: ids(3), focused: PhotoID(1), aspect: { _ in 1.5 }, in: CGSize(width: 4, height: 4))
        #expect(!tiny.isMulti && tiny.focusFrame.size == CGSize(width: 4, height: 4), "no room for tiles: single view")
    }

    @Test func largeSelectionsGetAMoreTile() {
        let all = ids(20)
        let s = MultiViewLayout.shown(all, focused: all[0])
        #expect(s.ids.count == MultiViewLayout.maxTiles - 1 && s.more == 9)
        let late = MultiViewLayout.shown(all, focused: all[15])
        #expect(late.ids.contains(all[15]) && late.ids.count == 11, "the focused photo is always shown")
        let twelve = MultiViewLayout.shown(ids(12), focused: nil)
        #expect(twelve.ids == ids(12) && twelve.more == 0, "up to the cap: every photo")
        let plan = MultiViewLayout.plan(ids: all, focused: all[15], aspect: { _ in 1.5 }, in: wide)
        #expect(plan.tiles.count == 12 && plan.photoCount == 11)
        #expect(plan.tiles.last?.content == .more(9))
        #expect(MultiViewLayout.moreLabel(9) == "+9 more")
    }

    @Test func voiceOverLabels() {
        #expect(MultiViewLayout.accessibilityLabel(position: 2, of: 4, focused: true) == "Photo 2 of 4, focused")
        #expect(MultiViewLayout.accessibilityLabel(position: 1, of: 4, focused: false) == "Photo 1 of 4")
    }
}

/// [multiview] Line snapping along each position's axis, the double-click toggle and the remembered layout.
@Suite struct FilmstripSizingTests {
    @Test func oneRowKeepsTheOldHeight() {
        #expect(FilmstripSizing.extent(lines: 1, at: .bottom) == 152, "the strip's height before it could grow")
        #expect(FilmstripSizing.extent(lines: 0, at: .bottom) == 0)
        #expect(FilmstripSizing.extent(lines: 3, at: .bottom) == 368, "two more rows of 108")
        // At the side: one column of 120-point cells plus side padding; each more is 124.
        #expect(FilmstripSizing.extent(lines: 1, at: .left) == 136)
        #expect(FilmstripSizing.extent(lines: 2, at: .right) == 260, "one more column of 124")
        #expect(FilmstripSizing.extent(lines: 0, at: .right) == 0)
    }

    @Test func dragsSnapToWholeRows() {
        let avail: CGFloat = 1000
        #expect(FilmstripSizing.lines(forExtent: 152, available: avail, at: .bottom) == 1)
        #expect(FilmstripSizing.lines(forExtent: 152 + 50, available: avail, at: .bottom) == 1, "under half a row: stays")
        #expect(FilmstripSizing.lines(forExtent: 152 + 60, available: avail, at: .bottom) == 2)
        #expect(FilmstripSizing.lines(forExtent: 152 + 216, available: avail, at: .bottom) == 3)
        #expect(FilmstripSizing.lines(forExtent: 80, available: avail, at: .bottom) == 1)
        #expect(FilmstripSizing.lines(forExtent: 70, available: avail, at: .bottom) == 0, "below half a row: collapsed")
        #expect(FilmstripSizing.lines(forExtent: -20, available: avail, at: .bottom) == 0)
    }

    @Test func sideDragsSnapToWholeColumns() {
        let avail: CGFloat = 1400
        #expect(FilmstripSizing.lines(forExtent: 136 + 60, available: avail, at: .left) == 1)
        #expect(FilmstripSizing.lines(forExtent: 136 + 64, available: avail, at: .right) == 2)
        #expect(FilmstripSizing.lines(forExtent: 60, available: avail, at: .left) == 0, "below half a column: collapsed")
        // 1400 − 320 viewer − 136 first column = 944 → 7 more columns of 124.
        #expect(FilmstripSizing.maxLines(available: avail, at: .left) == 8)
        #expect(FilmstripSizing.extent(lines: 8, at: .left) <= avail - FilmstripSizing.minViewerWidth)
        #expect(FilmstripSizing.extent(lines: 9, at: .left) > avail - FilmstripSizing.minViewerWidth)
        #expect(FilmstripSizing.lines(forExtent: 5000, available: avail, at: .right) == 8)
    }

    @Test func theViewerKeepsItsMinimumHeight() {
        // 1000 − 240 viewer − 152 first row = 608 → 5 more rows of 108.
        #expect(FilmstripSizing.maxLines(available: 1000, at: .bottom) == 6)
        #expect(FilmstripSizing.extent(lines: 6, at: .bottom) <= 1000 - FilmstripSizing.minViewerHeight)
        #expect(FilmstripSizing.extent(lines: 7, at: .bottom) > 1000 - FilmstripSizing.minViewerHeight)
        #expect(FilmstripSizing.lines(forExtent: 5000, available: 1000, at: .bottom) == 6)
        #expect(FilmstripSizing.maxLines(available: 200, at: .bottom) == 1, "a tiny window still shows a row")
        #expect(FilmstripSizing.clamped(9, available: 1000, at: .bottom) == 6)
        #expect(FilmstripSizing.clamped(0, available: 1000, at: .bottom) == 0)
    }

    @Test func columnsReflowToTheWidth() {
        let w = FilmstripSizing.cellWidth
        #expect(w == 120)
        #expect(FilmstripSizing.columns(width: 16 + 3 * w + 2 * 4) == 3)
        #expect(FilmstripSizing.columns(width: 16 + 3 * w + 2 * 4 - 1) == 2)
        #expect(FilmstripSizing.columns(width: 10) == 1)
        // A side strip of n columns is exactly wide enough for n.
        for n in 1...4 { #expect(FilmstripSizing.columns(width: FilmstripSizing.extent(lines: n, at: .left)) == n) }
    }

    @Test func doubleClickTogglesOneLineAndTheLastSize() {
        var s = FilmstripLayout()
        #expect(s.lines == 1 && s.last == 3 && s.position == .bottom)
        s = s.toggled
        #expect(s.lines == 3, "first double-click: the default multi-line size")
        s = s.dragged(to: 5)
        #expect(s == FilmstripLayout(lines: 5, last: 5))
        s = s.toggled
        #expect(s == FilmstripLayout(lines: 1, last: 5))
        #expect(s.toggled.lines == 5)
        s = s.dragged(to: 0)
        #expect(s.lines == 0 && s.last == 5, "collapsing keeps the last size")
        #expect(s.toggled.lines == 1, "collapsed → one line")
        #expect(FilmstripLayout(lines: -3, last: 0) == FilmstripLayout(lines: 0, last: 2), "clamped")
        let side = FilmstripLayout(position: .left, lines: 2, last: 4).moved(to: .right)
        #expect(side == FilmstripLayout(position: .right, lines: 2, last: 4), "moving keeps the size")
        #expect(side.toggled == FilmstripLayout(position: .right, lines: 1, last: 2))
    }

    @Test func positions() {
        #expect(FilmstripPosition.allCases.map(\.title) == ["Bottom", "Left", "Right"])
        #expect(!FilmstripPosition.bottom.isVertical && FilmstripPosition.left.isVertical && FilmstripPosition.right.isVertical)
        #expect(FilmstripText.size(lines: 0, vertical: true) == "Collapsed")
        #expect(FilmstripText.size(lines: 1, vertical: false) == "1 row")
        #expect(FilmstripText.size(lines: 3, vertical: true) == "3 columns")
    }

    @Test func savedLayoutsAreClamped() throws {
        let bad = Data(#"{"position":"left","lines":-4,"last":1}"#.utf8)
        #expect(try JSONDecoder().decode(FilmstripLayout.self, from: bad) == FilmstripLayout(position: .left, lines: 0, last: 2))
        let unknown = Data(#"{"position":"top","lines":2,"last":3}"#.utf8)
        #expect(try JSONDecoder().decode(FilmstripLayout.self, from: unknown).position == .bottom, "an unknown place: the bottom")
    }
}

/// [multiview] The model side: which photos the viewer shows, focus within the
/// selection, ↑/↓ across filmstrip rows, the toggle and the remembered size.
@MainActor
@Suite struct MultiViewModelTests {
    func started() async -> AppModel {
        let m = makeModel()
        await m.start()
        return m
    }

    @Test func selectionShowsInTheViewer() async {
        let m = await started()
        let v = m.visibleIDs
        #expect(m.multiViewIDs.isEmpty, "one photo: the single view")
        m.focus(v[0], extend: false)
        m.focus(v[3], extend: true)
        #expect(m.multiViewIDs == Array(v[0...3]), "in browser order")
        m.settings.showSelectionInViewer = false
        #expect(m.multiViewIDs.isEmpty, "the toggle keeps the single view")
        m.settings.showSelectionInViewer = true
        m.browserMode = .grid
        #expect(m.multiViewIDs.isEmpty, "the grid shows the selection itself")
        m.browserMode = .filmstrip
        m.viewer.togglePinCompare()
        #expect(m.multiViewIDs.isEmpty, "Compare pins its own pair")
        m.viewer.togglePinCompare()
        #expect(m.multiViewIDs.count == 4)
    }

    @Test func clickingATileFocusesItAndKeepsTheSelection() async {
        let m = await started()
        let v = m.visibleIDs
        m.focus(v[0], extend: false)
        m.focus(v[2], extend: true)
        m.focusWithinSelection(v[1])
        #expect(m.focusedID == v[1] && m.selection == Set(v[0...2]))
        m.focusWithinSelection(v[9])
        #expect(m.focusedID == v[1], "not selected: ignored")
        // Edit All Selected still edits every selected photo.
        m.editAllSelected = true
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 0.5 }
        #expect(v[0...2].allSatisfy { m.edit(for: $0).exposure.ev == 0.5 })
        #expect(m.edit(for: v[3]).exposure.ev == 0)
    }

    @Test func upAndDownMoveByAFilmstripRow() async {
        let m = await started()
        let v = m.visibleIDs
        m.focus(v[0], extend: false)
        m.perform(.move(.down, extend: false))
        #expect(m.focusedID == v[1], "one row: ↓ is the next photo")
        m.filmstrip = FilmstripLayout(lines: 3)
        m.filmstripColumns = 5
        m.perform(.move(.down, extend: false))
        #expect(m.focusedID == v[6])
        m.perform(.move(.up, extend: false))
        #expect(m.focusedID == v[1])
        m.perform(.move(.right, extend: false))
        #expect(m.focusedID == v[2], "←/→ still step one photo")
        m.filmstripHidden = true
        #expect(m.verticalStep == 1, "no strip on screen: one photo")
        m.filmstripHidden = false
        m.browserMode = .map
        #expect(m.verticalStep == 1)
        m.browserMode = .grid
        m.gridColumns = 4
        #expect(m.verticalStep == 4)
    }

    @Test func aSideStripNavigatesTheSameWay() async {
        let m = await started()
        let v = m.visibleIDs
        m.placeFilmstrip(.right)
        m.filmstripColumns = 1
        m.focus(v[0], extend: false)
        m.perform(.move(.down, extend: false))
        #expect(m.focusedID == v[1], "one column: ↓ runs down it")
        m.resizeFilmstrip(to: 2, ended: true)
        m.filmstripColumns = 2
        m.perform(.move(.down, extend: false))
        #expect(m.focusedID == v[3], "two columns: ↓ is the row below")
        m.perform(.move(.left, extend: false))
        #expect(m.focusedID == v[2], "←/→ across the columns")
        m.resizeFilmstrip(to: 0, ended: true)
        #expect(m.verticalStep == 1, "collapsed: one photo")
    }

    @Test func filmstripLayoutIsRememberedPerWindow() async {
        let m = await started()
        #expect(m.filmstrip == FilmstripLayout())
        m.resizeFilmstrip(to: 2, ended: false)
        #expect(m.filmstrip == FilmstripLayout(lines: 2, last: 3), "a live drag doesn't change the last size")
        m.resizeFilmstrip(to: 4, ended: true)
        #expect(m.filmstrip == FilmstripLayout(lines: 4, last: 4))
        m.toggleFilmstripLines()
        #expect(m.filmstrip.lines == 1)
        m.placeFilmstrip(.left)
        let reread = AppSettings(defaults: m.settings.userDefaults)
        #expect(reread.filmstripLayout(window: AppWindows.main) == FilmstripLayout(position: .left, lines: 1, last: 4))
        #expect(reread.filmstripLayout(window: "other") == FilmstripLayout(), "each window keeps its own")
        let again = AppModel(sessions: MockSessionStore(counts: [5]), images: MockImageProvider(latency: .zero),
                             renderer: FakeRenderer(), importer: MockImportService(bytesPerSecond: 1e12),
                             archive: UnavailableArchiveService(target: ""), ai: MockAIService(), settings: reread)
        #expect(again.filmstrip == FilmstripLayout(position: .left, lines: 1, last: 4), "restored at launch")
    }

    @Test func filmstripPositionCommands() async {
        let m = await started()
        #expect(CommandCatalog.filmstripMenu.map { CommandCatalog.title($0, model: m) } == ["Bottom", "Left", "Right"])
        #expect(CommandCatalog.filmstripMenu.allSatisfy { CommandCatalog.isEnabled($0, model: m) && $0.shortcut == nil })
        #expect(MenuCommand.zoomIn.filmstripPosition == nil)
        CommandCatalog.perform(.filmstripRight, model: m)
        #expect(m.filmstrip.position == .right)
        CommandCatalog.perform(.filmstripLeft, model: m)
        #expect(m.filmstrip.position == .left)
        CommandCatalog.perform(.filmstripBottom, model: m)
        #expect(m.filmstrip.position == .bottom)
    }

    @Test func showSelectionCommandAndControl() async {
        let m = await started()
        #expect(m.settings.showSelectionInViewer, "on by default")
        #expect(CommandCatalog.isEnabled(.showSelectionInViewer, model: m))
        #expect(CommandCatalog.title(.showSelectionInViewer, model: m) == "Show Selection in Viewer")
        #expect(ToolbarControl.showSelection.control(m).label == "Showing selection in viewer")
        CommandCatalog.perform(.showSelectionInViewer, model: m)
        #expect(!m.settings.showSelectionInViewer)
        #expect(ToolbarControl.showSelection.control(m).label == "Show selection in viewer")
        #expect(AppSettings(defaults: m.settings.userDefaults).showSelectionInViewer == false, "remembered")
        #expect(ToolbarControl.showSelection.control(m).hint?.isEmpty == false)
    }
}
