import CoreGraphics
import DarkslideAI
import Foundation

/// One image's people masks: per person (largest first) the Person matte and the six
/// sub-parts, E-frame, at the analysis image's size.
struct PeopleMasks: Sendable {
    let width: Int
    let height: Int
    let persons: [[AITarget: VisionMask]]
    /// Each person's box (normalized, the Person matte's extent): what an edit stores to
    /// find its people again (`EditState.personBoxes`, `PersonMatch`).
    let boxes: [CGRect]
    /// The faces were parsed by the face-parsing model (or there were none to parse):
    /// false for the colour-model fallback used while the model is unavailable.
    var parsed = true
    /// The faces were parsed on high-resolution crops of the photo (or there were none,
    /// or none needed them): false when only the analysis image was at hand.
    var detailed = true
    /// Detail renders were asked for: a result still not `detailed` then (the renders
    /// failed) isn't made again on every call.
    var detailTried = false
    /// …and asked for interactively: one asked for in the background only (queued behind
    /// the viewer, it may have given up) is tried once more for the photo on screen.
    var detailTriedInteractive = false

    init(width: Int, height: Int, parts: [PersonParts]) {
        self.width = width
        self.height = height
        persons = parts.map { p in p.masks.mapValues { VisionMask(width: width, height: height, bytes: $0) } }
        boxes = parts.map { Self.box(of: $0.masks[.person], width: width, height: height) }
        var all: [AITarget: VisionMask] = [:]
        for t in PeopleTargets.parts {
            let layers = parts.compactMap { $0.masks[t] }
            guard let first = layers.first else { continue }
            var m = first
            for l in layers.dropFirst() { for i in m.indices where l[i] > m[i] { m[i] = l[i] } }
            all[t] = VisionMask(width: width, height: height, bytes: m)
        }
        self.all = all
    }

    /// "All people": the per-pixel max over the persons.
    let all: [AITarget: VisionMask]

    /// `target` for person `person` (nil = all people). Nil when that person doesn't exist.
    func mask(_ target: AITarget, person: Int?) -> VisionMask? {
        guard let person else { return all[target] }
        return person >= 0 && person < persons.count ? persons[person][target] : nil
    }

    /// Normalized extent of a matte's pixels at ≥ 50 % (empty when none).
    static func box(of matte: [UInt8]?, width w: Int, height h: Int) -> CGRect {
        guard let matte, w > 0, h > 0 else { return .zero }
        var x0 = w, y0 = h, x1 = 0, y1 = 0
        for y in 0..<h {
            for x in 0..<w where matte[y * w + x] >= 128 {
                x0 = min(x0, x)
                x1 = max(x1, x + 1)
                y0 = min(y0, y)
                y1 = max(y1, y + 1)
            }
        }
        guard x1 > x0, y1 > y0 else { return .zero }
        return CGRect(x: Double(x0) / Double(w), y: Double(y0) / Double(h),
                      width: Double(x1 - x0) / Double(w), height: Double(y1 - y0) / Double(h))
    }
}

/// Finds an edit's people again: an edit stores each person's box when it masks them
/// (`EditState.personBoxes`); a later pass may order people differently (another OS's
/// Vision, the thumbnail instead of the preview), so indices are matched by box overlap.
enum PersonMatch {
    /// Least overlap (intersection over union) for "the same person".
    static let minOverlap = 0.3

    static func overlap(_ a: CGRect, _ b: CGRect) -> Double {
        let i = a.intersection(b)
        guard !i.isNull, !i.isEmpty else { return 0 }
        let inter = Double(i.width * i.height)
        let union = Double(a.width * a.height + b.width * b.height) - inter
        return union > 0 ? inter / union : 0
    }

    /// The current index of each stored person, best overlaps first and one person each;
    /// stored people with no match are absent.
    static func map(stored: [Int: CGRect], current: [CGRect]) -> [Int: Int] {
        var pairs: [(old: Int, new: Int, o: Double)] = []
        for (old, box) in stored {
            for (new, c) in current.enumerated() {
                let o = overlap(box, c)
                if o >= minOverlap { pairs.append((old, new, o)) }
            }
        }
        pairs.sort { $0.o != $1.o ? $0.o > $1.o : ($0.old, $0.new) < ($1.old, $1.new) }
        var out: [Int: Int] = [:], taken = Set<Int>()
        for p in pairs where out[p.old] == nil && !taken.contains(p.new) {
            out[p.old] = p.new
            taken.insert(p.new)
        }
        return out
    }

    /// The current index of `person`: found by its stored box, or the index itself for a
    /// shape made before boxes were stored. Nil when that person isn't found any more.
    static func resolve(_ person: Int, stored: [Int: CGRect], current: [CGRect]) -> Int? {
        guard stored[person] != nil else { return person }
        return map(stored: stored, current: current)[person]
    }
}

/// Computes `PeopleMasks` for an image (Vision + the face-parsing model) and keeps the
/// last few, so all-people and per-person shapes of a photo share one pass.
final class PeopleMaskService: @unchecked Sendable {
    /// A high-resolution render of a region (normalized, E-frame) of the photo whose
    /// analysis image is being masked, for the whole frame at `longEdge` pixels: the image
    /// and the region it actually covers. Faces are parsed on it rather than on the small
    /// analysis image.
    typealias FaceDetail = @Sendable (_ region: CGRect, _ longEdge: Int) async -> (image: CGImage, region: CGRect)?

    static let shared = PeopleMaskService(parser: { @MainActor in
        guard let model = try? await FaceParsingModel.shared.loadedModel() else { return nil }
        return CoreMLFaceParser(model: model)
    }, parserReady: { @MainActor in FaceParsingModel.shared.state == .ready })

    let observe: @Sendable (CGImage) throws -> PeopleObservation
    private let parser: @Sendable () async -> (any FaceParser)?
    /// Whether the parser is there now (without downloading it): a fallback result is
    /// computed again once it is.
    private let parserReady: @Sendable () async -> Bool
    private let lock = NSLock()
    private var cache: [(image: CGImage, masks: PeopleMasks)] = []
    private var running: [ObjectIdentifier: Task<PeopleMasks, Error>] = [:]
    private var counts: [(image: CGImage, count: Int)] = []
    let cacheLimit: Int
    private(set) var computed = 0

    init(observe: @escaping @Sendable (CGImage) throws -> PeopleObservation = PeopleVision.observe,
         parser: @escaping @Sendable () async -> (any FaceParser)?,
         parserReady: @escaping @Sendable () async -> Bool = { false }, cacheLimit: Int = 6) {
        self.observe = observe
        self.parser = parser
        self.parserReady = parserReady
        self.cacheLimit = cacheLimit
    }

    /// The masks of `image` (cached by image identity; one computation at a time per image).
    /// `detail` renders the photo's faces at high resolution for parsing. A cached result is
    /// computed again when it lacks what can now be had: the parser (the colour fallback
    /// was used while it was missing), or the detail renders (`interactive`: they are
    /// rendered for the photo on screen, not in the background).
    func masks(for image: CGImage, detail: FaceDetail? = nil, interactive: Bool = true) async throws -> PeopleMasks {
        let key = ObjectIdentifier(image)
        if let hit = lock.withLock({ cache.first(where: { $0.image === image })?.masks }) {
            let tried = hit.detailTried && (hit.detailTriedInteractive || !interactive)
            var stale = !hit.detailed && !tried && detail != nil
            if !stale, !hit.parsed { stale = await parserReady() }
            if stale { lock.withLock { cache.removeAll { $0.image === image } } }
        }
        let task: Task<PeopleMasks, Error> = lock.withLock {
            if let hit = cache.first(where: { $0.image === image }) { return Task { hit.masks } }
            if let t = running[key] { return t }
            let t = Task.detached(priority: .userInitiated) { [self] in try await compute(image, detail: detail, interactive: interactive) }
            running[key] = t
            return t
        }
        do {
            let m = try await task.value
            store(m, for: image)
            return m
        } catch {
            _ = lock.withLock { running.removeValue(forKey: key) }
            throw error
        }
    }

    private func store(_ m: PeopleMasks, for image: CGImage) {
        lock.withLock {
            running.removeValue(forKey: ObjectIdentifier(image))
            guard !cache.contains(where: { $0.image === image }) else { return }
            cache.append((image, m))
            if cache.count > cacheLimit { cache.removeFirst(cache.count - cacheLimit) }
        }
    }

    func cachedCount(_ image: CGImage) -> Int? {
        lock.withLock {
            cache.first { $0.image === image }?.masks.persons.count ?? counts.first { $0.image === image }?.count
        }
    }

    func rememberCount(_ n: Int, for image: CGImage) {
        lock.withLock {
            counts.removeAll { $0.image === image }
            counts.append((image, n))
            if counts.count > cacheLimit { counts.removeFirst(counts.count - cacheLimit) }
        }
    }

    /// `target` of `image` for one person or all (nil when there's no such person).
    func mask(_ target: AITarget, person: Int?, image: CGImage, detail: FaceDetail? = nil) async throws -> VisionMask? {
        try await masks(for: image, detail: detail).mask(target, person: person)
    }

    private func compute(_ image: CGImage, detail: FaceDetail?, interactive: Bool) async throws -> PeopleMasks {
        guard let pixels = PixelImage(image) else { throw PeopleMaskError.pixelBuffer }
        let observation = try observe(image)
        var parses: [FaceParse?] = []
        let needsParse = !observation.faces.isEmpty && !observation.mattes.isEmpty
        var parsed = !needsParse, detailed = true
        if needsParse, let parser = await parser() {
            parsed = true
            for f in observation.faces {
                let crop = FaceCrop.around(face: f.box, angle: f.angle, size: parser.inputSize)
                let (rgba, sharp) = await Self.faceInput(crop, pixels: pixels, detail: detail)
                detailed = detailed && sharp
                parses.append((try? parser.parse(rgba: rgba)).map { FaceParse(crop: crop, labels: $0) })
            }
        }
        lock.withLock { computed += 1 }
        var masks = PeopleMasks(width: observation.width, height: observation.height,
                                parts: PeopleComposer.compose(image: pixels, observation: observation, parses: parses))
        masks.parsed = parsed
        masks.detailed = detailed
        masks.detailTried = detail != nil
        masks.detailTriedInteractive = detail != nil && interactive
        return masks
    }

    /// Detail renders are asked for faces this much smaller in the analysis image than the
    /// parser's input (a crop upscaled less than that is sharp enough as it is).
    static let detailFactor = 1.25

    /// The parser's input for `crop`: sampled from a high-resolution render of its region
    /// when the analysis image would be upscaled (and `detail` can render one), else from
    /// the analysis image. Also whether it is as sharp as can be had.
    static func faceInput(_ crop: FaceCrop, pixels: PixelImage, detail: FaceDetail?) async -> (rgba: [UInt8], sharp: Bool) {
        let w = pixels.width, h = pixels.height
        let long = Double(max(w, h))
        // Whole-frame long edge at which the crop has the parser's resolution.
        let want = Int((long * detailFactor * Double(crop.size) / crop.side).rounded(.up))
        guard Double(want) > long * detailFactor, let b = crop.frameBounds(width: w, height: h) else {
            return (crop.extract(from: pixels), true)
        }
        let region = CGRect(x: Double(b.x0) / Double(w), y: Double(b.y0) / Double(h),
                            width: Double(b.x1 - b.x0) / Double(w), height: Double(b.y1 - b.y0) / Double(h))
        guard let detail else { return (crop.extract(from: pixels), false) }
        guard let d = await detail(region, min(want, 16384)), let px = PixelImage(d.image) else {
            return (crop.extract(from: pixels), false)
        }
        return (crop.extract(from: px, region: d.region, frameWidth: w, frameHeight: h), true)
    }
}

/// The face-parsing model (`DownloadableModelDescriptor.faceParsing`), downloaded the
/// first time a people sub-part is masked.
enum FaceParsingModel {
    @MainActor static let shared = DownloadableModel.standard(.faceParsing)
}
