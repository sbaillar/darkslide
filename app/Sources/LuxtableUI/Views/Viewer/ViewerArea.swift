import LuxtableCore
import SwiftUI

/// [multiview] The viewer, or with several photos selected (View ▸ Show Selection in
/// Viewer) all of them in a grid. The focused tile *is* the live viewer (zoom, pan,
/// before/after, tools): it stays one view that only moves, so the Metal surface is
/// never rebuilt. The other tiles show the edited thumbnails from the cache.
package struct ViewerArea: View {
    @Bindable var model: AppModel

    package init(model: AppModel) {
        self.model = model
    }

    package var body: some View {
        GeometryReader { geo in
            let plan = MultiViewLayout.plan(ids: model.multiViewIDs, focused: model.focusedID,
                                            aspect: { model.photo($0)?.meta.aspectRatio ?? 1.5 }, in: geo.size)
            let count = plan.photoCount
            ZStack(alignment: .topLeading) {
                Color.clear
                if plan.isMulti {
                    ForEach(plan.tiles, id: \.content) { tile in
                        switch tile.content {
                        case .photo(let id) where id != model.focusedID:
                            if let item = model.item(id) {
                                MultiViewTile(item: item, size: tile.frame.size, cache: model.thumbnails,
                                              label: MultiViewLayout.accessibilityLabel(position: plan.position(of: id) ?? 0,
                                                                                        of: count, focused: false))
                                    .onTapGesture { model.focusWithinSelection(id) }
                                    .contextMenu { CullContextMenu(model: model) }
                                    .position(x: tile.frame.midX, y: tile.frame.midY)
                            }
                        case .more(let n):
                            MoreTile(count: n) { model.browserMode = .grid }
                                .frame(width: tile.frame.width, height: tile.frame.height)
                                .position(x: tile.frame.midX, y: tile.frame.midY)
                        default:
                            EmptyView()
                        }
                    }
                }
                let f = plan.focusFrame
                ViewerContainer(model: model)
                    .frame(width: f.width, height: f.height)
                    .overlay {
                        if plan.isMulti {
                            RoundedRectangle(cornerRadius: DS.Radius.s)
                                .strokeBorder(DS.accent, lineWidth: 2.5)
                                .allowsHitTesting(false)
                        }
                    }
                    .clipShape(.rect(cornerRadius: plan.isMulti ? DS.Radius.s : 0))
                    .accessibilityElement(children: .contain)
                    .accessibilityLabel(plan.isMulti
                        ? MultiViewLayout.accessibilityLabel(position: model.focusedID.flatMap(plan.position) ?? 1, of: count, focused: true)
                        : "Viewer")
                    .position(x: f.midX, y: f.midY)
            }
            .onAppear { model.viewerAreaWidth = geo.size.width }
            .onChange(of: geo.size.width) { _, w in model.viewerAreaWidth = w }
        }
        .background(DS.viewerBackground)
    }
}

/// [multiview] One selected, unfocused photo: its edited thumbnail at the cheapest
/// cached size that fills the tile. A click focuses it (the selection stays).
struct MultiViewTile: View {
    /// Observed per item: a rating on this photo re-renders only this tile.
    let item: PhotoItem
    let size: CGSize
    let label: String
    @Environment(\.displayScale) private var displayScale
    @State private var loader: ThumbnailLoader

    init(item: PhotoItem, size: CGSize, cache: ThumbnailCache, label: String) {
        self.item = item
        self.size = size
        self.label = label
        _loader = State(initialValue: ThumbnailLoader(cache: cache))
    }

    var body: some View {
        let photo = item.photo
        let rect = ThumbnailLayout.fit(aspect: photo.meta.aspectRatio, in: size, inset: DS.Space.s)
        let bucket = ThumbnailLoader.bucket(points: max(rect.width, rect.height), scale: displayScale)
        ZStack(alignment: .topLeading) {
            DS.viewerBackground
            Group {
                if let image = loader.image {
                    Image(decorative: image, scale: 1).resizable().interpolation(.high)
                } else {
                    Rectangle().fill(Color.primary.opacity(0.07))
                }
            }
            .frame(width: rect.width, height: rect.height)
            .offset(x: rect.minX, y: rect.minY)
            HStack(spacing: DS.Space.xs) {
                Text(photo.filename).lineLimit(1)
                StarsView(rating: photo.rating, size: 9)
            }
            .font(.system(size: 10, weight: .medium))
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(.black.opacity(0.6), in: .capsule)
            .foregroundStyle(.white)
            .frame(width: size.width, height: size.height, alignment: .bottom)
            .padding(.bottom, DS.Space.s)
        }
        .frame(width: size.width, height: size.height)
        .overlay { RoundedRectangle(cornerRadius: DS.Radius.s).strokeBorder(DS.hairline, lineWidth: 1) }
        .contentShape(.rect)
        .help("\(photo.filename): click to make it the photo you edit")
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(label)
        .accessibilityValue(PhotoAccessibility.label(photo))
        .accessibilityAddTraits([.isButton, .isSelected])
        .onAppear { loader.prime(photo, bucket: bucket) }
        // The key includes the photo's edit: an edit (Edit All Selected) reloads it.
        .task(id: loader.key(for: photo, bucket: bucket)) { await loader.load(photo, bucket: bucket) }
    }
}

/// [multiview] "+N more": the selection is larger than the viewer shows; a click opens the grid.
struct MoreTile: View {
    let count: Int
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(MultiViewLayout.moreLabel(count))
                .font(DS.Font.title)
                .foregroundStyle(DS.textSecondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(DS.cellBackground.opacity(0.55), in: .rect(cornerRadius: DS.Radius.s))
        }
        .buttonStyle(.plain)
        .help("Show the whole selection in the grid")
        .accessibilityLabel("\(count) more selected photos")
        .accessibilityHint("Shows the grid")
    }
}
