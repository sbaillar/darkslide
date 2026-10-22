import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation

/// A people target for one person (`MaskShape.aiPerson`).
public struct PersonMaskKey: Hashable, Sendable {
    public let target: AITarget
    public let person: Int

    public init(target: AITarget, person: Int) {
        self.target = target
        self.person = person
    }
}

/// Gathers per-person mask bitmaps for a photo (the per-person twin of
/// `DevelopRenderer.MaskProvider`); a missing one renders empty.
public typealias PersonMaskProvider = @Sendable (PhotoID, Set<PersonMaskKey>) async -> [PersonMaskKey: VisionMask]

/// A photo's lens-blur depth map; `true` waits for it to be computed (export).
public typealias DepthProvider = @Sendable (PhotoID, Bool) async -> DepthResult?

/// The app's depth provider for every render path (viewer, edited thumbnails, export);
/// nil (tests, demo) renders depth lens blur as nothing.
public enum DepthWiring {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var _provider: DepthProvider?

    public static var provider: DepthProvider? {
        get { lock.withLock { _provider } }
        set { lock.withLock { _provider = newValue } }
    }
}

extension AITarget {
    init(_ t: DevelopAiTarget) {
        self = switch t {
        case .subject: .subject
        case .sky: .sky
        case .person: .person
        case .background: .background
        case .face: .face
        case .skin: .skin
        case .eyes: .eyes
        case .teeth: .teeth
        case .hair: .hair
        case .clothes: .clothes
        }
    }
}

extension DevelopMask {
    init(_ m: VisionMask) {
        self.init(width: UInt32(m.width), height: UInt32(m.height), data: Data(m.bytes), version: m.version)
    }
}

extension DevelopRenderer {
    /// Per-person AI shapes of the enabled layers.
    static func personKeys(in edit: EditState) -> Set<PersonMaskKey> {
        var out = Set<PersonMaskKey>()
        for layer in edit.layers where layer.enabled {
            for shape in layer.mask.shapes {
                if case .aiPerson(_, let t, let p) = shape { out.insert(PersonMaskKey(target: t, person: p)) }
            }
        }
        return out
    }
}

extension StaticMasks {
    /// The masks `edit` needs for `photo`: whole-image targets from `provider`, per-person
    /// ones from `persons`. Nil when the edit has no AI shapes or nothing provides them.
    /// A depth lens blur adds the photo's depth map from `depth` (never waited for: a
    /// missing map is started and the photo renders again when it lands), and its subject
    /// matte as the subject mask when no layer asked for one (the pipeline's auto focus).
    static func gather(_ edit: EditState, photo: PhotoID, provider: DevelopRenderer.MaskProvider?,
                       persons: PersonMaskProvider?, depth: DepthProvider? = DepthWiring.provider,
                       waitForDepth: Bool = false) async -> StaticMasks? {
        let targets = DevelopRenderer.aiTargets(in: edit)
        let keys = DevelopRenderer.personKeys(in: edit)
        var all: [AITarget: VisionMask] = [:]
        var each: [PersonMaskKey: VisionMask] = [:]
        if !targets.isEmpty, let provider { all = await provider(photo, targets) }
        if !keys.isEmpty, let persons { each = await persons(photo, keys) }
        var map: DepthResult?
        if edit.effects.lensBlur.needsDepth, let depth { map = await depth(photo, waitForDepth) }
        if let s = map?.subject, all[.subject] == nil { all[.subject] = s }
        guard (!targets.isEmpty && provider != nil) || (!keys.isEmpty && persons != nil) || map != nil else { return nil }
        return StaticMasks(all, persons: each, depth: map?.depth)
    }
}

extension AIMasker {
    /// People targets (all people) through `PeopleMaskService`.
    static func people(_ target: AITarget, _ image: CGImage) async throws -> VisionMask? {
        try await PeopleMaskService.shared.mask(target, person: nil, image: image)
    }
}

extension ViewerModel {
    /// Per-person masks for a render: computed with the photo's preview (cached by the
    /// people service), faces parsed on high-resolution renders. Each person is found
    /// again by the box the edit stored (`PersonMatch`); a person the photo doesn't have
    /// (any more) is left out (renders empty) and, on the photo on screen, reported once.
    func personMasksForRender(photo: PhotoID, keys: Set<PersonMaskKey>) async -> [PersonMaskKey: VisionMask] {
        guard let app, let p = app.photo(photo), let img = await app.analysisImage(for: p) else { return [:] }
        let boxes = app.edit(for: photo).personBoxes
        let got = await Self.personMasks(keys, image: img, service: peopleService, boxes: boxes, detail: faceDetail(for: p),
                                         interactive: Self.faceDetailPriority(photo: photo, focused: app.focusedID) == .interactive)
        let lost = Set(keys.filter { got[$0] == nil && boxes[$0.person] != nil }.map(\.person))
        if photo == app.focusedID, !lost.isEmpty { reportLostPeople(lost, photo: photo) }
        return got
    }

    static func personMasks(_ keys: Set<PersonMaskKey>, image: CGImage, service: PeopleMaskService,
                            boxes: [Int: CGRect] = [:],
                            detail: PeopleMaskService.FaceDetail? = nil,
                            interactive: Bool = true) async -> [PersonMaskKey: VisionMask] {
        guard let masks = try? await service.masks(for: image, detail: detail, interactive: interactive) else { return [:] }
        var out: [PersonMaskKey: VisionMask] = [:]
        for k in keys {
            guard let now = PersonMatch.resolve(k.person, stored: boxes, current: masks.boxes) else { continue }
            out[k] = masks.mask(k.target, person: now)
        }
        return out
    }

    /// Renders of `photo`'s faces at high resolution for face parsing (the develop
    /// renderer's region renders, the photo as shot); nil when the renderer can't.
    /// Interactive only for the photo on screen: for thumbnails and exports they queue
    /// behind the viewer and never pin their RAW (which would unpin the viewer's).
    func faceDetail(for photo: Photo) -> PeopleMaskService.FaceDetail? {
        guard capabilities.regions, capabilities.rendersEdits else { return nil }
        let renderer = renderer
        let priority = Self.faceDetailPriority(photo: photo.id, focused: app?.focusedID)
        return { region, longEdge in
            let request = RenderRequest(maxPixelSize: longEdge, region: region, quality: .full, priority: priority,
                                        geometry: .oriented)
            guard let frame = try? await renderer.render(photo, edit: EditState(), request: request),
                  let image = frame.makeCGImage() else { return nil }
            return (image, frame.region ?? region)
        }
    }

    nonisolated static func faceDetailPriority(photo: PhotoID, focused: PhotoID?) -> RenderPriority {
        photo == focused ? .interactive : .background
    }

    /// "Person 2 isn't found …", once per photo and person.
    func reportLostPeople(_ people: Set<Int>, photo: PhotoID) {
        let new = people.filter { reportedLostPeople.insert("\(photo.raw)/\($0)").inserted }
        guard let app, !new.isEmpty else { return }
        let who = new.sorted().map { "Person \($0 + 1)" }.joined(separator: " and ")
        app.show(Toast("\(who) isn't found in this photo any more: those people masks are empty.",
                       symbol: "person.crop.circle.badge.questionmark", isError: true))
    }

    /// How many people the focused photo has (for the Layers panel's person picker),
    /// computing its people masks if needed; 0 when unknown.
    public func peopleCount() async -> Int {
        guard let app, let p = app.focusedPhoto, let img = await app.analysisImage(for: p),
              let m = try? await peopleService.masks(for: img) else { return 0 }
        return m.persons.count
    }
}

extension AppModel {
    /// Wire per-person masks into the develop renderer, the edited thumbnails and export.
    func wirePersonMasks(develop: DevelopRenderer) {
        let provider: PersonMaskProvider = { [weak self] id, keys in
            guard let self else { return [:] }
            return await self.viewer.personMasksForRender(photo: id, keys: keys)
        }
        develop.personMaskProvider = provider
        (tools as? RustDevelopTools)?.personMaskProvider = provider
        (exporter as? RustExportService).map { e in
            var e = e
            e.personMaskProvider = provider
            exporter = e
        }
    }
}

extension RustExportService {
    /// Per-person masks of every photo whose edit has per-person shapes, by source id.
    func gatherPersonMasks(_ items: [UInt64: ExportJobItem]) async -> [UInt64: [PersonMaskKey: VisionMask]] {
        guard let personMaskProvider else { return [:] }
        var out: [UInt64: [PersonMaskKey: VisionMask]] = [:]
        for (id, item) in items {
            guard let edit = item.edit else { continue }
            let keys = DevelopRenderer.personKeys(in: edit)
            if !keys.isEmpty { out[id] = await personMaskProvider(item.photoID, keys) }
        }
        return out
    }
}
