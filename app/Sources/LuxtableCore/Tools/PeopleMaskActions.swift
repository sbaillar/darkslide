import CoreGraphics
import LuxtableAI
import Foundation

extension AppModel {
    /// Add a people mask (Person, Face, Skin, Eyes, Teeth, Hair or Clothes) for one person
    /// (index, largest first) or all people (nil) to layer `layerID` (or a new layer) of
    /// the focused photo. Returns the layer id; throws a user-facing error when that
    /// person or part isn't in the photo.
    @discardableResult
    func addPeopleMask(_ target: AITarget, person: Int?, toLayer layerID: Int?,
                              service: PeopleMaskService? = nil) async throws -> Int {
        guard let photo = focusedPhoto, let image = await analysisImage(for: photo) else { throw AIMaskError.noImage }
        let service = service ?? viewer.peopleService
        let people = try await service.masks(for: image, detail: viewer.faceDetail(for: photo))
        guard let mask = people.mask(target, person: person), mask.coverage > 0.0005 else {
            throw AIMaskError.nothingFound(target)
        }
        let shape = PeopleTargets.shape(target, person: person)
        let name = PeopleTargets.label(target, person: person)
        var added = layerID ?? -1
        updateEdits([photo.id], label: "AI \(name) Mask") { _, e in
            if let person, person < people.boxes.count {
                // The edit's people in today's order first, so `person` means one person only.
                e.adoptPeopleOrder(people.boxes)
                e.personBoxes[person] = people.boxes[person]
            }
            added = LayerOps.addShape(shape, toLayer: layerID, newLayerName: name, in: &e.layers)
        }
        if person == nil { viewer.masks.set(mask, photo: photo.id, target: target) }
        return added
    }
}

extension LayerController {
    /// People mask on the selected layer (or a new one), then select it.
    public func addPeopleMask(_ target: AITarget, person: Int?) async throws {
        let layer = try await app.addPeopleMask(target, person: person, toLayer: selected)
        select(layer)
    }

    /// People in the focused photo (Vision only, no model download); 0 when unknown.
    public func peopleCount() async -> Int {
        guard let p = app.focusedPhoto, let img = await app.analysisImage(for: p) else { return 0 }
        return (try? await app.viewer.peopleService.personCount(for: img)) ?? 0
    }
}

extension EditState {
    /// Renumber the per-person shapes that stored a box to the people order `current`
    /// (normalized boxes, `PeopleMasks.boxes`), so an index added now can't mean two
    /// people. People not found any more move past the current ones (they render empty,
    /// and keep their box should they be found again).
    mutating func adoptPeopleOrder(_ current: [CGRect]) {
        guard !personBoxes.isEmpty else { return }
        var map = PersonMatch.map(stored: personBoxes, current: current)
        var next = current.count
        for old in personBoxes.keys.sorted() where map[old] == nil {
            map[old] = next
            next += 1
        }
        guard map.contains(where: { $0.key != $0.value }) else { return }
        for l in layers.indices {
            layers[l].mask.shapes = layers[l].mask.shapes.map { shape in
                guard case .aiPerson(let op, let t, let p) = shape, let q = map[p] else { return shape }
                return .aiPerson(op, t, person: q)
            }
        }
        personBoxes = Dictionary(uniqueKeysWithValues: personBoxes.map { (map[$0.key] ?? $0.key, $0.value) })
    }
}

/// The people part of the Layers panel: target buttons, the person picker and the face
/// parsing model's download (logic only; `PeopleMaskMenu` draws it).
public enum PeopleMenu {
    /// "All People", then "Person 1" … "Person n".
    public static func personChoices(count: Int) -> [(label: String, person: Int?)] {
        [("All People", nil)] + (0..<max(0, count)).map { ("Person \($0 + 1)", $0) }
    }

    public static func symbol(_ t: AITarget) -> String {
        switch t {
        case .person: "person.fill"
        case .face: "face.smiling"
        case .skin: "hand.raised"
        case .eyes: "eye"
        case .teeth: "mouth"
        case .hair: "comb"
        case .clothes: "tshirt"
        default: "sparkles"
        }
    }

    /// Tooltip of a people target button.
    @MainActor
    public static func help(_ t: AITarget, person: Int?) -> String {
        let who = person.map { "person \($0 + 1)" } ?? "every person"
        let base = "Mask the \(t.rawValue) of \(who) on this Mac, on the selected layer (or a new one)."
        guard t != .person, FaceParsingModel.shared.state != .ready else { return base }
        return base + " The first time, a 53 MB face model is downloaded."
    }

    /// The download line under the buttons (nil when there's nothing to show).
    @MainActor
    public static var downloadStatus: (fraction: Double, text: String)? {
        if case .downloading(let f) = FaceParsingModel.shared.state {
            return (f, "Downloading the face model (BiSeNet face parsing)…")
        }
        return nil
    }

    @MainActor
    public static func cancelDownload() { FaceParsingModel.shared.cancel() }
}

extension PeopleMaskService {
    /// How many people Vision finds (no face parsing); cached with the image.
    func personCount(for image: CGImage) async throws -> Int {
        if let hit = cachedCount(image) { return hit }
        let n = try await Task.detached(priority: .userInitiated) { [observe] in try observe(image).mattes.count }.value
        rememberCount(n, for: image)
        return n
    }
}
