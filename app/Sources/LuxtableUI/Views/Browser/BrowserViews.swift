import LuxtableCore
import SwiftUI

enum ThumbnailStyle {
    /// Compact strip under the viewer: a film-edge line under each frame, filename on hover.
    case filmstrip
    /// Library grid: filename always shown under the image.
    case grid
}

/// One thumbnail with culling badges. Loading is delegated to `ThumbnailLoader`
/// (cache hit applied synchronously; load cancelled with the cell's task).
struct ThumbnailCell: View {
    /// Observed per item: a rating on this photo re-renders only this cell.
    let item: PhotoItem
    let size: CGSize
    let style: ThumbnailStyle
    let isSelected: Bool
    let isFocused: Bool
    /// Member count when this cell stands for a collapsed burst stack.
    var stackCount: Int?
    var onToggleStack: (() -> Void)?
    /// the grid's caption stars rate on click.
    var onRate: ((Int) -> Void)?
    @Environment(\.displayScale) private var displayScale
    @State private var loader: ThumbnailLoader
    @State private var hovering = false

    init(item: PhotoItem, size: CGSize, style: ThumbnailStyle, isSelected: Bool, isFocused: Bool, cache: ThumbnailCache,
         stackCount: Int? = nil, onToggleStack: (() -> Void)? = nil, onRate: ((Int) -> Void)? = nil) {
        self.item = item
        self.size = size
        self.style = style
        self.isSelected = isSelected
        self.isFocused = isFocused
        self.stackCount = stackCount
        self.onToggleStack = onToggleStack
        self.onRate = onRate
        _loader = State(initialValue: ThumbnailLoader(cache: cache))
    }

    private var photo: Photo { item.photo }
    private var captionHeight: CGFloat { style == .grid ? 20 : 13 }
    private var imageBox: CGSize { CGSize(width: size.width, height: size.height - captionHeight) }

    var body: some View {
        let bucket = ThumbnailLoader.bucket(points: max(imageBox.width, imageBox.height), scale: displayScale)
        let rect = ThumbnailLayout.fit(aspect: photo.meta.aspectRatio, in: imageBox, inset: DS.Space.s)
        VStack(spacing: 0) {
            ZStack(alignment: .topLeading) {
                if stackCount != nil { stackShadow(rect) }
                imageLayer
                    .frame(width: rect.width, height: rect.height)
                    .offset(x: rect.minX, y: rect.minY)
                badges(rect)
                if photo.isVideo { // clips: play badge; Space opens Quick Look
                    Image(systemName: "play.circle.fill")
                        .font(.system(size: min(28, rect.height * 0.3)))
                        .foregroundStyle(.white, .black.opacity(0.45))
                        .frame(width: rect.width, height: rect.height)
                        .offset(x: rect.minX, y: rect.minY)
                        .allowsHitTesting(false)
                        .accessibilityLabel("Video")
                }
                if photo.isDamaged {
                    Label("Damaged", systemImage: "exclamationmark.triangle.fill")
                        .font(.system(size: 10, weight: .semibold))
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(DS.reject.opacity(0.85), in: .capsule)
                        .foregroundStyle(.white)
                        .frame(width: imageBox.width, height: imageBox.height, alignment: .top)
                        .padding(.top, DS.Space.xs)
                        .help(OpenFailures.damagedMessage)
                } else if !photo.variantName.isEmpty { // a variant says so
                    Text(photo.variantName)
                        .font(.system(size: 9, weight: .semibold))
                        .padding(.horizontal, 5).padding(.vertical, 1)
                        .background(.black.opacity(0.6), in: .capsule)
                        .foregroundStyle(.white)
                        .frame(width: imageBox.width, height: imageBox.height, alignment: .top)
                        .padding(.top, DS.Space.xs)
                }
                if let text = MissingActions.badge(photo, missing: item.originalMissing) {
                    Label(text, systemImage: item.isOriginalMissing ? "questionmark.folder" : "exclamationmark.arrow.triangle.2.circlepath")
                        .font(.system(size: 10, weight: .semibold))
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(DS.reject.opacity(0.85), in: .capsule)
                        .foregroundStyle(.white)
                        .frame(width: imageBox.width, height: imageBox.height, alignment: .bottom)
                        .padding(.bottom, DS.Space.xs)
                }
            }
            .frame(width: imageBox.width, height: imageBox.height, alignment: .topLeading)
            if style == .grid { caption } else { edgePrint }
        }
        .frame(width: size.width, height: size.height)
        .background(background, in: .rect(cornerRadius: DS.Radius.s))
        .overlay {
            RoundedRectangle(cornerRadius: DS.Radius.s)
                .strokeBorder(isFocused ? DS.accent : (isSelected ? DS.accent.opacity(0.55) : DS.hairline),
                              lineWidth: isFocused ? 2.5 : 1)
        }
        .shadow(color: isFocused ? DS.accent.opacity(0.35) : .clear, radius: 4)
        .onHover { hovering = $0 }
        .help("\(photo.filename) · \(Formatters.shutter(photo.meta.shutter)) \(Formatters.aperture(photo.meta.aperture)) ISO \(photo.meta.iso)")
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(PhotoAccessibility.label(photo, stackCount: stackCount))
        .accessibilityAddTraits(PhotoAccessibility.isSelected(isSelected, isFocused) ? [.isButton, .isSelected] : .isButton)
        .accessibilityAction(named: style == .filmstrip ? "Show burst frames" : "Expand or collapse stack") { onToggleStack?() }
        .modifier(AdjustableRating(rating: photo.rating, onRate: onRate))
        .onAppear { loader.prime(photo, bucket: bucket) }
        // Keyed on photo + bucket only: rating/tag/pick changes never reload the image.
        // The key includes the photo's edit (live mode), so a paste or a slider reloads it.
        .task(id: loader.key(for: photo, bucket: bucket)) { await loader.load(photo, bucket: bucket) }
    }

    private var background: Color {
        if isFocused { return DS.accent.opacity(0.20) }
        if isSelected { return DS.accent.opacity(0.10) }
        return DS.cellBackground.opacity(0.55)
    }

    @ViewBuilder private var imageLayer: some View {
        if let image = loader.image {
            Image(decorative: image, scale: 1)
                .resizable()
                .interpolation(.high)
                .clipShape(.rect(cornerRadius: 2))
                .shadow(color: .black.opacity(0.4), radius: 2, y: 1)
                .saturation(photo.pick == .rejected ? 0.15 : 1)
                .opacity(photo.pick == .rejected ? 0.35 : 1)
        } else {
            RoundedRectangle(cornerRadius: 2)
                .fill(Color.primary.opacity(0.07))
                .overlay {
                    if loader.failed {
                        Image(systemName: "exclamationmark.triangle").foregroundStyle(DS.textSecondary)
                            .help("Couldn’t load a preview")
                    }
                }
        }
    }

    private func stackShadow(_ rect: CGRect) -> some View {
        ForEach(1...2, id: \.self) { i in
            RoundedRectangle(cornerRadius: 2)
                .fill(Color.primary.opacity(0.18 - Double(i) * 0.05))
                .frame(width: rect.width, height: rect.height)
                .offset(x: rect.minX + CGFloat(i) * 3, y: rect.minY - CGFloat(i) * 3)
        }
    }

    private func badges(_ rect: CGRect) -> some View {
        let inner = rect.insetBy(dx: 4, dy: 4)
        return ZStack(alignment: .topLeading) {
            // Grid only: the filmstrip prints flag, stars and "edited" on its film edge.
            // Top-left: pick flag.
            if style == .grid { flag.position(x: inner.minX + 8, y: inner.minY + 8) }
            // Top-right: developed copy for Immich (left of the edited badge).
            if let copy = item.immichCopy {
                Image(systemName: copy.symbol)
                    .font(.system(size: 9, weight: .bold))
                    .padding(3)
                    .background(.black.opacity(0.6), in: .rect(cornerRadius: 3))
                    .foregroundStyle(copy == .current ? Color.white : DS.star)
                    .position(x: inner.maxX - (style == .grid && photo.isEdited ? 29 : 9), y: inner.minY + 9)
                    .help(copy.help)
                    .accessibilityLabel(copy.help)
            }
            // Top-right: edited badge.
            if style == .grid, photo.isEdited {
                Image(systemName: "slider.horizontal.3")
                    .font(.system(size: 9, weight: .bold))
                    .padding(3)
                    .background(.black.opacity(0.6), in: .rect(cornerRadius: 3))
                    .foregroundStyle(.white)
                    .position(x: inner.maxX - 9, y: inner.minY + 9)
                    .help("Adjusted")
            }
            // Bottom-right: color tag chip, stack badge.
            HStack(spacing: 4) {
                if let stackCount {
                    Button(action: { onToggleStack?() }) {
                        HStack(spacing: 2) {
                            Image(systemName: "square.stack.3d.up.fill").font(.system(size: 9))
                            Text("\(stackCount)").font(.system(size: 10, weight: .bold)).monospacedDigit()
                        }
                        .padding(.horizontal, 5).padding(.vertical, 2)
                        .background(DS.accent, in: .capsule)
                        .foregroundStyle(.black)
                    }
                    .buttonStyle(.plain)
                    .help(style == .filmstrip ? "Burst of \(stackCount). Click to see its frames and choose its cover"
                          : "Burst of \(stackCount). Click to expand (S)")
                }
                if photo.colorTag != .none {
                    TagChip(tag: photo.colorTag)
                }
            }
            .fixedSize()
            .frame(width: inner.width, alignment: .trailing)
            .position(x: inner.midX, y: inner.maxY - 9)
            // Filmstrip: filename on hover.
            if style == .filmstrip, hovering {
                Text(photo.filename)
                    .font(.system(size: 10, weight: .medium))
                    .lineLimit(1)
                    .padding(.horizontal, 5).padding(.vertical, 2)
                    .background(.black.opacity(0.7), in: .capsule)
                    .foregroundStyle(.white)
                    .fixedSize()
                    .position(x: inner.midX, y: inner.midY)
                    .transition(.opacity)
            }
        }
        .allowsHitTesting(stackCount != nil)
    }

    @ViewBuilder private var flag: some View {
        switch photo.pick {
        case .picked:
            Image(systemName: "flag.fill").font(.system(size: 11))
                .foregroundStyle(DS.pick)
                .padding(3).background(.black.opacity(0.55), in: .circle)
                .help("Picked")
        case .rejected:
            Image(systemName: "xmark").font(.system(size: 10, weight: .heavy))
                .foregroundStyle(.white)
                .padding(4).background(DS.reject, in: .circle)
                .help(AICullApplication.rejectHelp(photo))
        case .none: EmptyView()
        }
    }

    /// Filmstrip: frame number, stars, flag and "edited" in amber type on the film edge
    /// (rejects in the reject colour).
    private var edgePrint: some View {
        Text(FilmEdge.caption(photo))
            .font(.system(size: 9, weight: .medium, design: .monospaced))
            .foregroundStyle(photo.pick == .rejected ? DS.reject : DS.accent.opacity(0.9))
            .lineLimit(1)
            .truncationMode(.tail)
            .padding(.horizontal, DS.Space.s)
            .frame(width: size.width, height: captionHeight, alignment: .leading)
            .offset(y: -2)
    }

    private var caption: some View {
        HStack(spacing: DS.Space.xs) {
            Text(photo.filename)
                .font(.system(size: 11))
                .foregroundStyle(.primary.opacity(0.85))
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 0)
            if let onRate {
                ClickableStars(rating: photo.rating, size: 10, onRate: onRate)
            } else {
                StarsView(rating: photo.rating, size: 10)
            }
        }
        .padding(.horizontal, DS.Space.s)
        .frame(height: captionHeight)
    }
}

/// Filled color chip with a dark ring so it reads on any image.
package struct TagChip: View {
    let tag: ColorTag

    package init(tag: ColorTag) { self.tag = tag }

    package var body: some View {
        RoundedRectangle(cornerRadius: 3)
            .fill(DS.tagColor(tag))
            .frame(width: 14, height: 10)
            .overlay(RoundedRectangle(cornerRadius: 3).strokeBorder(.black.opacity(0.6), lineWidth: 1))
            .help("\(tag.name) tag")
            .accessibilityLabel("\(tag.name) tag")
    }
}

package struct StarsView: View {
    let rating: Int
    var size: CGFloat = 10

    package init(rating: Int, size: CGFloat = 10) {
        self.rating = rating
        self.size = size
    }

    package var body: some View {
        if rating > 0 {
            HStack(spacing: 1) {
                ForEach(0..<rating, id: \.self) { _ in
                    Image(systemName: "star.fill").font(.system(size: size))
                }
            }
            .foregroundStyle(DS.star)
            .accessibilityLabel(RatingText.label(rating)) // "1 star"
        }
    }
}

/// the grid caption's five stars; a click rates (the same star clears).
struct ClickableStars: View {
    let rating: Int
    var size: CGFloat = 10
    let onRate: (Int) -> Void
    @State private var hover: Int?

    var body: some View {
        HStack(spacing: 1) {
            ForEach(1...5, id: \.self) { n in
                let filled = n <= (hover ?? rating)
                Image(systemName: filled ? "star.fill" : "star")
                    .font(.system(size: size))
                    .foregroundStyle(filled ? DS.star : Color.primary.opacity(hover == nil ? 0.18 : 0.4))
                    .contentShape(.rect)
                    .onHover { hover = $0 ? n : (hover == n ? nil : hover) }
                    .onTapGesture { onRate(RatingText.rating(afterClicking: n, current: rating)) }
            }
        }
        .help("Click a star to rate; click the same star again to clear")
        .accessibilityElement()
        .accessibilityLabel("Rating")
        .accessibilityValue(RatingText.label(rating))
        .accessibilityAdjustableAction { dir in
            onRate(min(max(rating + (dir == .increment ? 1 : -1), 0), 5))
        }
    }
}

/// VoiceOver's rating control for a grid cell: increment / decrement rate it (the caption
/// stars the cell hides from VoiceOver rate on click).
private struct AdjustableRating: ViewModifier {
    let rating: Int
    let onRate: ((Int) -> Void)?

    func body(content: Content) -> some View {
        if let onRate {
            content.accessibilityAdjustableAction { dir in
                onRate(PhotoAccessibility.adjustedRating(rating, increment: dir == .increment))
            }
        } else {
            content
        }
    }
}

/// Strip under (or beside) the viewer. Lazy, keeps the focused image in view.
/// [multiview] With more than one line (rows at the bottom, columns at the side) it is a
/// grid that reflows to its size (still lazy).
package struct Filmstrip: View {
    @Bindable var model: AppModel
    var lines = 1
    var position = FilmstripPosition.bottom
    /// The grid's cell height (the layout's size, fitted to the window).
    var cell = FilmstripSizing.defaultCellHeight
    /// Where the collapsed stacks' cells are, for the burst strip to fan out from.
    @State private var anchors = StackAnchors()

    package init(model: AppModel, lines: Int = 1, position: FilmstripPosition = .bottom,
                 cell: CGFloat = FilmstripSizing.defaultCellHeight) {
        self.model = model
        self.lines = lines
        self.position = position
        self.cell = cell
    }

    private var gridded: Bool { position.isVertical || lines > 1 }

    package var body: some View {
        let browser = model.browserState
        VStack(spacing: 0) {
            if position.isVertical {
                // A column is narrower than the bar: it scrolls sideways.
                ScrollView(.horizontal) { FilterBar(model: model, compact: true) }.scrollIndicators(.never)
            } else {
                FilterBar(model: model, compact: true)
            }
            Divider()
            if model.visibleIDs.isEmpty {
                Text(model.currentSession == nil ? "No session open" : model.emptyState.title)
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .contextMenu { FilmstripPositionMenu(model: model) }
            } else {
                SprocketRow()
                GeometryReader { geo in
                    let ids = browser.displayIDs(model.visibleIDs, lookup: { model.item($0)?.photo })
                    ScrollViewReader { proxy in
                        Group {
                            if gridded {
                                let cols = position.isVertical ? max(lines, 1) : FilmstripSizing.columns(width: geo.size.width, cell: cell)
                                let size = CGSize(width: FilmstripSizing.cellWidth(cell), height: cell)
                                ScrollView(.vertical) {
                                    LazyVGrid(columns: Array(repeating: GridItem(.fixed(size.width), spacing: FilmstripSizing.spacing), count: cols),
                                              spacing: FilmstripSizing.spacing) {
                                        ForEach(ids, id: \.self) { id in cellView(id, size: size) }
                                    }
                                    .padding(.horizontal, FilmstripSizing.sidePadding)
                                    .padding(.vertical, FilmstripSizing.padding)
                                }
                                .onAppear { model.filmstripColumns = cols }
                                .onChange(of: cols) { _, c in model.filmstripColumns = c }
                            } else {
                                let h = geo.size.height - 2 * DS.Space.xxs
                                let cell = CGSize(width: ThumbnailLayout.filmstripCellWidth(height: h), height: h)
                                ScrollView(.horizontal) {
                                    LazyHStack(spacing: DS.Space.xs) {
                                        ForEach(ids, id: \.self) { id in cellView(id, size: cell) }
                                    }
                                    .padding(.horizontal, DS.Space.s)
                                    .padding(.vertical, DS.Space.xxs)
                                }
                            }
                        }
                        .scrollIndicators(gridded ? .automatic : .never)
                        .onChange(of: model.focusedID) { _, id in
                            guard let id else { return }
                            withAnimation(.snappy(duration: 0.18)) { proxy.scrollTo(id, anchor: .center) }
                        }
                        .onChange(of: lines) { if let id = model.focusedID { proxy.scrollTo(id, anchor: .center) } }
                        .onChange(of: position) { if let id = model.focusedID { proxy.scrollTo(id, anchor: .center) } }
                        .onChange(of: cell) { if let id = model.focusedID { proxy.scrollTo(id, anchor: .center) } }
                    }
                }
                SprocketRow()
            }
        }
        .background(DS.chrome)
        .onAppear { browser.refresh(photos: model.visiblePhotos) }
        .onChange(of: model.visibleIDs) { browser.refresh(photos: model.visiblePhotos) }
    }

    @ViewBuilder private func cellView(_ id: PhotoID, size: CGSize) -> some View {
        let browser = model.browserState
        if let item = model.item(id) {
            let badge = browser.stacks.badgeCount(for: id)
            // A collapsed burst whose strip is open stays highlighted while its frames are shown.
            let stripHere = badge != nil && browser.picker.stackID != nil && browser.stacks.stack(of: id)?.id == browser.picker.stackID
            ThumbnailCell(item: item, size: size, style: .filmstrip,
                          isSelected: model.selection.contains(id), isFocused: model.focusedID == id || stripHere,
                          cache: model.thumbnails, stackCount: badge,
                          onToggleStack: { model.toggleBurstStrip(for: id, anchor: anchors.frames[id]) })
                .id(id)
                .modifier(StackAnchorReporter(id: id, anchors: anchors, enabled: badge != nil))
                .onTapGesture {
                    BrowserClick.handle(id, model: model)
                    if badge != nil, !PlatformInput.commandDown, !PlatformInput.shiftDown {
                        model.openBurstStrip(for: id, anchor: anchors.frames[id])
                    }
                }
                .contextMenu {
                    CullContextMenu(model: model)
                    if model.canUseAsCover(id) {
                        Button("Use as Stack Cover", systemImage: "checkmark.seal") { model.useAsCover(id) }
                    }
                    BestShotMenuItem(model: model, id: id)
                    Divider()
                    Menu("Filmstrip") { FilmstripPositionMenu(model: model) } // [multiview]
                }
                .modifier(MapDragSource(model: model, id: id)) // [map] geotag by dropping on the map
        }
    }
}

/// A burst's right-click Merge to Best Shot…: that burst's frames, whatever is selected.
private struct BestShotMenuItem: View {
    let model: AppModel
    let id: PhotoID

    var body: some View {
        if model.canMergeBestShot(ofStackOf: id) {
            Button("Merge to Best Shot…", systemImage: "sparkles.rectangle.stack") { model.mergeBestShot(ofStackOf: id) }
        }
    }
}

/// Window frames of the filmstrip's collapsed-stack cells. A plain box: recording a
/// frame while scrolling never re-renders the strip.
final class StackAnchors {
    var frames: [PhotoID: CGRect] = [:]
}

/// Records a collapsed stack cell's frame (window coordinates) in `anchors`.
private struct StackAnchorReporter: ViewModifier {
    let id: PhotoID
    let anchors: StackAnchors
    let enabled: Bool

    func body(content: Content) -> some View {
        if enabled {
            content.onGeometryChange(for: CGRect.self, of: { $0.frame(in: .global) }, action: { anchors.frames[id] = $0 })
        } else {
            content
        }
    }
}

/// [multiview] Bottom / Left / Right, the current one checked (View menu and the strip's context menu).
package struct FilmstripPositionMenu: View {
    @Bindable var model: AppModel

    package init(model: AppModel) { self.model = model }

    package var body: some View {
        ForEach(CommandCatalog.filmstripMenu) { c in
            Toggle(CommandCatalog.title(c, model: model),
                   isOn: Binding(get: { model.filmstrip.position == c.filmstripPosition },
                                 set: { _ in CommandCatalog.perform(c, model: model) }))
        }
    }
}

/// [multiview] The line between the viewer and the filmstrip: drag it to show more rows
/// (or columns, beside the viewer), snapped to whole lines, down to collapsed; a
/// double-click toggles one line ↔ the last size.
package struct FilmstripSeparator: View {
    @Bindable var model: AppModel
    /// Lines shown now, and the length the viewer and the strip share along the axis.
    let lines: Int
    let available: CGFloat
    /// The thumbnails' cell height: lines snap to rows (columns) of this size.
    let cell: CGFloat
    @State private var startExtent: CGFloat?

    package init(model: AppModel, lines: Int, available: CGFloat, cell: CGFloat) {
        self.model = model
        self.lines = lines
        self.available = available
        self.cell = cell
    }

    private var position: FilmstripPosition { model.filmstrip.position }
    private var vertical: Bool { position.isVertical }

    package var body: some View {
        let thick: CGFloat = lines == 0 ? 8 : 1
        ZStack {
            Rectangle().fill(lines == 0 ? DS.chrome : Color.clear)
            if vertical { Divider().frame(maxHeight: .infinity) } else { Divider() }
            if lines == 0 {
                Capsule().fill(DS.textTertiary).frame(width: vertical ? 3 : 36, height: vertical ? 36 : 3)
            }
        }
        .frame(width: vertical ? thick : nil, height: vertical ? nil : thick)
        .frame(maxWidth: vertical ? nil : .infinity, maxHeight: vertical ? .infinity : nil)
        // A larger grab area than the line itself.
        .overlay {
            Color.clear.frame(width: vertical ? 9 : nil, height: vertical ? nil : 9).contentShape(.rect)
                .gesture(drag)
                .onTapGesture(count: 2) { model.toggleFilmstripLines() }
                .pointerStyle(vertical ? .columnResize : .rowResize)
        }
        .zIndex(1)
        .help(vertical ? "Drag to show more columns of thumbnails; double-click for one column or your last width"
              : "Drag to show more rows of thumbnails; double-click for one row or your last size")
        .accessibilityElement()
        .accessibilityLabel("Filmstrip size")
        .accessibilityValue(FilmstripText.size(lines: lines, vertical: vertical))
        .accessibilityAdjustableAction { dir in
            let n = FilmstripSizing.clamped(max(lines + (dir == .increment ? 1 : -1), 0), available: available, at: position,
                                            cell: cell)
            model.resizeFilmstrip(to: n, ended: true)
        }
        .accessibilityAction(named: vertical ? "Toggle one column" : "Toggle one row") { model.toggleFilmstripLines() }
    }

    /// How far the strip grew: up at the bottom, right for a left strip, left for a right one.
    private func grown(_ t: CGSize) -> CGFloat {
        switch position {
        case .bottom: -t.height
        case .left: t.width
        case .right: -t.width
        }
    }

    private var drag: some Gesture {
        DragGesture(minimumDistance: 1, coordinateSpace: .global)
            .onChanged { v in
                let start = startExtent ?? FilmstripSizing.extent(lines: lines, at: position, cell: cell)
                if startExtent == nil { startExtent = start }
                let n = FilmstripSizing.lines(forExtent: start + grown(v.translation), available: available, at: position,
                                              cell: cell)
                if n != model.filmstrip.lines { model.resizeFilmstrip(to: n, ended: false) }
            }
            .onEnded { v in
                let start = startExtent ?? FilmstripSizing.extent(lines: lines, at: position, cell: cell)
                model.resizeFilmstrip(to: FilmstripSizing.lines(forExtent: start + grown(v.translation), available: available,
                                                                at: position, cell: cell), ended: true)
                startExtent = nil
            }
    }
}

/// A faint row of sprocket holes along the filmstrip's edge.
struct SprocketRow: View {
    var body: some View {
        Canvas { ctx, size in
            var holes = Path()
            for x in FilmEdge.sprocketXs(width: Double(size.width)) {
                holes.addRoundedRect(in: CGRect(x: CGFloat(x), y: (size.height - 4) / 2, width: 6, height: 4),
                                     cornerSize: CGSize(width: 1, height: 1))
            }
            ctx.fill(holes, with: .color(.primary.opacity(0.11)))
        }
        .frame(height: 8)
        .accessibilityHidden(true)
    }
}

/// Click routing shared by grid and filmstrip.
enum BrowserClick {
    @MainActor static func handle(_ id: PhotoID, model: AppModel) {
        ControlFocus.shared.resignAll() // clicking a photo hands the arrows back to culling
        if PlatformInput.commandDown { model.toggleSelection(id) } else { model.focus(id, extend: PlatformInput.shiftDown) }
    }
}

/// Full grid (Library view). Lazy and virtualized: only visible cells exist
/// and load thumbnails; scrolling 2,000 items stays smooth.
package struct BrowserGrid: View {
    @Bindable var model: AppModel
    @State private var cellSize: CGFloat = 180

    package init(model: AppModel) {
        self.model = model
    }

    package var body: some View {
        let browser = model.browserState
        VStack(spacing: 0) {
            GridHeader(model: model, cellSize: $cellSize)
            Divider()
            FilterBar(model: model, compact: false)
            Divider()
            if model.visibleIDs.isEmpty {
                ViewerEmptyState(model: model)
            } else {
                GeometryReader { geo in
                    let spacing = DS.Space.s
                    let cols = max(1, Int((geo.size.width - 2 * DS.Space.m + spacing) / (cellSize + spacing)))
                    let cell = CGSize(width: cellSize, height: (cellSize * 0.8).rounded() + 20)
                    ScrollViewReader { proxy in
                        ScrollView {
                            LazyVGrid(columns: Array(repeating: GridItem(.fixed(cellSize), spacing: spacing), count: cols),
                                      spacing: spacing) {
                                ForEach(browser.displayIDs(model.visibleIDs, lookup: { model.item($0)?.photo }), id: \.self) { id in
                                    if let item = model.item(id) {
                                        ThumbnailCell(item: item, size: cell, style: .grid,
                                                      isSelected: model.selection.contains(id), isFocused: model.focusedID == id,
                                                      cache: model.thumbnails, stackCount: browser.stacks.badgeCount(for: id),
                                                      onToggleStack: { browser.stacks.toggleStack(of: id) },
                                                      onRate: { n in model.rate(id, n) })
                                            .id(id)
                                            .onTapGesture { BrowserClick.handle(id, model: model) }
                                            .contextMenu {
                                                CullContextMenu(model: model)
                                                BestShotMenuItem(model: model, id: id)
                                            }
                                            .simultaneousGesture(TapGesture(count: 2).onEnded {
                                                model.focus(id, extend: false)
                                                model.browserMode = .filmstrip
                                            })
                                    }
                                }
                            }
                            .padding(DS.Space.m)
                        }
                        .onAppear {
                            model.gridColumns = cols
                            if let f = model.focusedID { proxy.scrollTo(f, anchor: .center) }
                        }
                        .onChange(of: cols) { _, c in model.gridColumns = c }
                        .onChange(of: model.focusedID) { _, id in
                            guard let id else { return }
                            withAnimation(.snappy(duration: 0.15)) { proxy.scrollTo(id) }
                        }
                    }
                }
            }
        }
        .background(DS.viewerBackground)
        .onAppear { browser.refresh(photos: model.visiblePhotos) }
        .onChange(of: model.visibleIDs) { browser.refresh(photos: model.visiblePhotos) }
    }
}

/// Grid header: session, stacking, thumbnail size.
struct GridHeader: View {
    @Bindable var model: AppModel
    @Binding var cellSize: CGFloat

    var body: some View {
        let browser = model.browserState
        HStack(spacing: DS.Space.m) {
            Text(model.currentSession?.name ?? "Library").font(.system(size: 13, weight: .semibold))
            Spacer()
            Toggle(isOn: Binding(get: { browser.stackBursts }, set: { model.setStackBursts($0) })) {
                Label("Stack Bursts", systemImage: "square.stack.3d.up")
            }
            .toggleStyle(.button)
            .help("Collapse bursts (frames < 1 s apart) into stacks")
            Image(systemName: "square.grid.3x3").foregroundStyle(DS.textSecondary)
            Slider(value: $cellSize, in: 110...320)
                .frame(width: 120)
                .help("Thumbnail size")
            Image(systemName: "square.grid.2x2").foregroundStyle(DS.textSecondary)
        }
        .controlSize(.small)
        .padding(.horizontal, DS.Space.m)
        .frame(height: 36)
        .background(DS.chrome)
    }
}
