import MapKit
import SwiftUI

/// [map] The Map mode: clustered markers for where photos were taken. Logic
/// lives in `MapModel`; this view only reports the camera and forwards clicks,
/// drawn areas and drops. Tiles load from Apple Maps only once the user has
/// allowed it (asked on first use). Offline, MapKit is expected to show a plain
/// background with the markers (not verified on a disconnected Mac).
struct MapBrowser: View {
    @Bindable var model: AppModel
    @Bindable var map: MapModel

    var body: some View {
        VStack(spacing: 0) {
            MapHeader(model: model, map: map)
            Divider()
            switch map.tiles {
            case .on: MapCanvas(model: model, map: map)
            case .ask: TilesNotice(map: map, asking: true)
            case .off: TilesNotice(map: map, asking: false)
            }
        }
        // The header's count follows the photos with or without a map.
        .onAppear { map.refresh() }
        .onChange(of: model.photosVersion) { _, _ in map.refresh() }
        .onChange(of: model.filter) { _, _ in map.refresh() }
    }
}

/// The first time (or with tiles turned off): say where tiles come from before any load.
private struct TilesNotice: View {
    @Bindable var map: MapModel
    let asking: Bool

    var body: some View {
        VStack(spacing: DS.Space.m) {
            Image(systemName: "map").font(.system(size: 36)).foregroundStyle(DS.textSecondary)
            Text(MapModel.tilesNotice)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
            if !asking {
                Text("The map is off. You can turn it on here or in Settings ▸ General.")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
            }
            HStack {
                if asking { Button("Not Now") { map.allowTiles(false) } }
                Button("Show Map") { map.allowTiles(true) }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(DS.Space.l)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(DS.viewerBackground)
    }
}

/// The map itself (only once tiles are allowed).
private struct MapCanvas: View {
    @Bindable var model: AppModel
    @Bindable var map: MapModel
    @State private var position: MapCameraPosition = .automatic
    @State private var dragStart: CGPoint?
    @State private var dragNow: CGPoint?
    @State private var dropTargeted = false

    var body: some View {
        GeometryReader { geo in
            MapReader { proxy in
                let surface = MapKitSurface(proxy: proxy, size: geo.size)
                // Pan and zoom only: on a rotated or pitched map a screen rectangle isn't a lat/lon box.
                Map(position: $position, interactionModes: map.drawingArea ? [] : [.pan, .zoom]) {
                    ForEach(map.clusters) { c in
                        Annotation("", coordinate: c.location.coordinate, anchor: .bottom) {
                            MapMarker(model: model, map: map, cluster: c)
                                .onTapGesture { map.select(c) }
                        }
                    }
                }
                .mapStyle(.standard(pointsOfInterest: .excludingAll))
                .onMapCameraChange(frequency: .onEnd) { ctx in
                    map.cameraChanged(GeoRegion(ctx.region), size: geo.size)
                }
                .dropDestination(for: PhotoDragItem.self) { items, location in
                    items.reduce(false) { $0 || map.drop($1, at: location, on: surface) }
                } isTargeted: { dropTargeted = $0 }
                .overlay { if map.drawingArea { drawLayer(surface) } }
                .overlay {
                    if dropTargeted {
                        RoundedRectangle(cornerRadius: DS.Radius.s).strokeBorder(DS.accent, lineWidth: 3)
                            .allowsHitTesting(false)
                    }
                }
            }
        }
        .overlay(alignment: .topTrailing) {
            if !map.remoteChoice.isEmpty { RemoteChoice(map: map).padding(DS.Space.m) }
        }
        .overlay(alignment: .center) {
            if map.clusters.isEmpty, map.loading != .loading { emptyNote }
        }
        .onAppear { map.appeared() }
        .onChange(of: map.cameraRequest) { _, _ in
            if let r = map.requestedRegion { withAnimation { position = .region(r.mapKit) } }
        }
        .onChange(of: model.focusedID) { _, _ in map.focusChanged() }
    }

    private var emptyNote: some View {
        Text(map.scope == .session
             ? "No photo here has a location. Drag photos from the filmstrip onto the map to set one."
             : "No photo here has a location yet.")
            .font(DS.Font.label)
            .foregroundStyle(DS.textSecondary)
            .padding(DS.Space.m)
            .background(DS.chrome.opacity(0.9), in: .rect(cornerRadius: DS.Radius.s))
            .allowsHitTesting(false)
    }

    /// Dragging draws the area to filter to.
    private func drawLayer(_ surface: MapKitSurface) -> some View {
        Color.black.opacity(0.001)
            .overlay {
                if let a = dragStart, let b = dragNow {
                    Rectangle()
                        .fill(DS.accent.opacity(0.12))
                        .overlay(Rectangle().stroke(DS.accent, lineWidth: 2))
                        .frame(width: abs(b.x - a.x), height: abs(b.y - a.y))
                        .position(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2)
                }
            }
            .gesture(DragGesture(minimumDistance: 4)
                .onChanged { v in
                    dragStart = v.startLocation
                    dragNow = v.location
                }
                .onEnded { v in
                    map.drewArea(from: v.startLocation, to: v.location, on: surface)
                    dragStart = nil
                    dragNow = nil
                })
    }
}

/// MapKit's projection as the model's `MapSurface`.
struct MapKitSurface: MapSurface {
    let proxy: MapProxy
    let size: CGSize

    func location(at point: CGPoint) -> GeoPoint? {
        proxy.convert(point, from: .local).flatMap { GeoPoint(latitude: $0.latitude, longitude: $0.longitude) }
    }
}

/// Scope, framing, filters and geotag removal above the map.
private struct MapHeader: View {
    @Bindable var model: AppModel
    @Bindable var map: MapModel

    var body: some View {
        HStack(spacing: DS.Space.m) {
            Picker("Show", selection: $map.scope) {
                ForEach(MapModel.Scope.allCases, id: \.self) { Text($0.rawValue) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            .help("Photos of this session, or of every session")
            Button("Show All", systemImage: "arrow.up.left.and.arrow.down.right") { map.showAll() }
                .disabled(map.tiles != .on)
                .help("Frame every photo on the map")
            Button("Show Selected", systemImage: "scope") { map.showSelection() }
                .disabled(map.tiles != .on)
                .help("Frame the selected photos")
            Group { // nothing to see or draw on without the map
                Toggle(isOn: $map.filterToView) { Label("Filter to View", systemImage: "rectangle.dashed") }
                    .toggleStyle(.button)
                    .help("Show only the photos in view in the filmstrip as the map moves")
                Toggle(isOn: $map.drawingArea) { Label("Draw Area", systemImage: "square.dashed") }
                    .toggleStyle(.button)
                    .help("Drag on the map to show only the photos inside")
            }
            .disabled(map.tiles != .on)
            let removable = map.removableSelection
            if !removable.isEmpty {
                Button("Remove Location", systemImage: "mappin.slash") { map.removeLocation(of: removable) }
                    .help("Remove the position set on the map (the file's own GPS, if any, shows again)")
            }
            Spacer(minLength: 0)
            switch map.loading {
            case .loading: ProgressView().controlSize(.small)
            case .failed(let m): Text(m).foregroundStyle(DS.reject).lineLimit(1)
            case .idle: EmptyView()
            }
            Text("\(map.locatedCount.formatted()) of \(model.photoCount.formatted()) located")
                .font(DS.Font.value)
                .foregroundStyle(DS.textSecondary)
        }
        .labelStyle(.titleAndIcon)
        .font(DS.Font.label)
        .controlSize(.small)
        .padding(.horizontal, DS.Space.m)
        .frame(height: 34)
        .background(DS.chrome)
    }
}

/// One marker: the first photo's thumbnail, with a count when it holds several.
private struct MapMarker: View {
    @Bindable var model: AppModel
    let map: MapModel
    let cluster: MapCluster
    @State private var loader: ThumbnailLoader?

    var body: some View {
        let photo = cluster.photoIDs.first.flatMap(model.photo)
        // Read here, so a selection change re-renders markers, never the clustering.
        let highlighted = map.isHighlighted(cluster)
        ZStack(alignment: .topTrailing) {
            Group {
                if let img = loader?.image {
                    Image(decorative: img, scale: 1).resizable().scaledToFill()
                } else {
                    Image(systemName: photo == nil ? "photo.on.rectangle.angled" : "photo")
                        .foregroundStyle(DS.textSecondary)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .background(DS.cellBackground)
                }
            }
            .frame(width: 44, height: 44)
            .clipShape(.rect(cornerRadius: 6))
            .overlay {
                RoundedRectangle(cornerRadius: 6).strokeBorder(highlighted ? DS.accent : .white, lineWidth: highlighted ? 3 : 1.5)
            }
            .shadow(radius: 2)
            if cluster.count > 1 {
                Text(cluster.count.formatted())
                    .font(.system(size: 10, weight: .bold))
                    .padding(.horizontal, 5).padding(.vertical, 1)
                    .background(DS.accent, in: .capsule)
                    .foregroundStyle(.black)
                    .offset(x: 8, y: -8)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(cluster.count) photo\(cluster.count == 1 ? "" : "s")")
        .accessibilityAddTraits(.isButton)
        .task(id: photo?.id) {
            guard let photo else { return }
            let l = loader ?? ThumbnailLoader(cache: model.thumbnails)
            loader = l
            await l.load(photo, bucket: ThumbnailLoader.bucket(points: 44, scale: 2))
        }
    }
}

/// Other sessions' photos at a clicked marker: open their session.
private struct RemoteChoice: View {
    @Bindable var map: MapModel

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Text("In other sessions").font(DS.Font.panelTitle).foregroundStyle(DS.textSecondary)
            ForEach(MapModel.sessions(map.remoteChoice), id: \.root) { s in
                Button("\(s.name) (\(s.count))") {
                    let pins = map.remoteChoice.filter { $0.sessionRoot == s.root }
                    Task { await map.open(pins) }
                }
                .help("Open \(s.name) and show these photos")
            }
        }
        .padding(DS.Space.m)
        .background(DS.chrome, in: .rect(cornerRadius: DS.Radius.s))
        .shadow(radius: 4)
    }
}

/// A filmstrip cell carries the selection (or itself) onto the map.
struct MapDragSource: ViewModifier {
    @Bindable var model: AppModel
    let id: PhotoID

    func body(content: Content) -> some View {
        if model.browserMode == .map, model.currentSession != nil {
            // Built when the drag starts, not on every render.
            content.draggable(model.map.dragItem(from: id) ?? PhotoDragItem(ids: [], session: UUID()))
        } else {
            content
        }
    }
}

extension GeoPoint {
    var coordinate: CLLocationCoordinate2D { CLLocationCoordinate2D(latitude: latitude, longitude: longitude) }
}

extension GeoRegion {
    init(_ r: MKCoordinateRegion) {
        let c = GeoPoint(latitude: r.center.latitude, longitude: r.center.longitude) ?? GeoPoint(latitude: 0, longitude: 0)!
        self.init(center: c, latitudeDelta: r.span.latitudeDelta, longitudeDelta: r.span.longitudeDelta)
    }

    var mapKit: MKCoordinateRegion {
        MKCoordinateRegion(center: center.coordinate,
                           span: MKCoordinateSpan(latitudeDelta: latitudeSpan, longitudeDelta: longitudeSpan))
    }
}
