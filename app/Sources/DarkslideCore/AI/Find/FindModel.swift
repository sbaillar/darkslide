import DarkslideFFI
import Foundation
import Observation

/// The store side of Find (seam for tests).
public protocol FindBackend: Sendable {
    /// What to embed for `text` (its description and place terms).
    func prompts(_ text: String, today: FindDay, model: String) async throws -> FindPrompts
    func find(_ text: String, today: FindDay, model: String, textVector: [Float]?, placeVectors: [FindPlaceVector],
              options: FindOptionsRecord) async throws -> FindResults
}

/// `FindBackend` over the global index and the people store through the FFI.
public struct RustFindBackend: FindBackend {
    public let globalIndex: URL
    public let peopleStore: URL

    public init(globalIndex: URL = RustImportService.defaultGlobalIndex, peopleStore: URL = AppPaths.peopleStore) {
        self.globalIndex = globalIndex
        self.peopleStore = peopleStore
    }

    public func prompts(_ text: String, today: FindDay, model: String) async throws -> FindPrompts {
        let (g, p) = (globalIndex.path, peopleStore.path)
        return try await RustSessionStore.call { token in
            try await findPrompts(text: text, globalIndex: g, peopleStore: p, today: today, model: model, token: token)
        }
    }

    public func find(_ text: String, today: FindDay, model: String, textVector: [Float]?, placeVectors: [FindPlaceVector],
                     options: FindOptionsRecord) async throws -> FindResults {
        let (g, p) = (globalIndex.path, peopleStore.path)
        return try await RustSessionStore.call { token in
            try await findPhotos(text: text, globalIndex: g, peopleStore: p, today: today, model: model,
                                 textVector: textVector, placeVectors: placeVectors, options: options, token: token)
        }
    }
}

/// Find (⌘F): "pics of alex in iceland last summer" across every session.
/// People, places, dates and ratings resolved against the library filter;
/// the description ranks (MobileCLIP, when the search model is installed).
/// Results: sessions (by name, folder, keyword, or many hits) and photos, each
/// saying why it matched.
@MainActor @Observable
public final class FindModel {
    public enum Phase: Equatable, Sendable {
        case idle
        case searching
        case results
        case failed(String)
    }

    /// A query part or a match reason, as the panel shows it.
    public struct Chip: Equatable, Sendable, Hashable {
        public enum Kind: Sendable, Hashable { case person, suggested, place, looksLike, date, rating, flag, visual, text }
        public var kind: Kind
        public var text: String
    }

    public var isPresented = false
    public var query = ""
    public private(set) var phase: Phase = .idle
    public private(set) var results: FindResults?
    /// The description couldn't be matched visually (no search model on this Mac).
    public private(set) var visualUnavailable = false
    public var options = FindOptionsRecord(limit: 300, minScore: 0.18, window: 0.09, placeWeight: 0.5)

    @ObservationIgnored public private(set) var backend: (any FindBackend)?
    @ObservationIgnored weak var app: AppModel?
    @ObservationIgnored var today: () -> Date = { Date() }
    @ObservationIgnored private var ticket = 0
    @ObservationIgnored private var task: Task<Void, Never>?

    public init() {}

    public func configure(backend: any FindBackend) {
        self.backend = backend
    }

    func attach(_ app: AppModel) {
        self.app = app
    }

    public var isAvailable: Bool { backend != nil }

    /// ⌘F.
    public func present() {
        guard isAvailable else { return }
        isPresented = true
    }

    public func dismiss() {
        isPresented = false
    }

    // MARK: - Searching

    /// Search as the user types (debounced).
    public func queryChanged(debounce: Duration = .milliseconds(350)) {
        task?.cancel()
        task = Task { [weak self] in
            try? await Task.sleep(for: debounce)
            guard !Task.isCancelled else { return }
            await self?.run()
        }
    }

    /// Return in the field.
    public func submit() {
        queryChanged(debounce: .zero)
    }

    /// Wait for the pending search (tests).
    func waitForSearch() async {
        await task?.value
    }

    func day() -> FindDay {
        let c = Calendar.current.dateComponents([.year, .month, .day], from: today())
        return FindDay(year: Int32(c.year ?? 1970), month: UInt32(c.month ?? 1), day: UInt32(c.day ?? 1))
    }

    public func run() async {
        let text = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let backend, !text.isEmpty else { return clear() }
        ticket += 1
        let mine = ticket
        phase = .searching
        do {
            await syncOpenSession()
            let day = day()
            let search = app?.search
            // The image-text model when installed (no download from here).
            let encoder: (any SearchEncoding)? = if let search, search.models?.isReady == true || search.hasEncoder {
                try? await search.readyEncoder()
            } else {
                nil
            }
            let model = encoder?.modelID ?? ClipModelDescriptor.mobileCLIPS2.id
            let prompts = try await backend.prompts(text, today: day, model: model)
            var textVector: [Float]?
            var places: [FindPlaceVector] = []
            if let encoder {
                (textVector, places) = try await Task.detached(priority: .userInitiated) {
                    let t = try prompts.text.map { try encoder.textEmbedding(PhotoSearch.prompt($0)) }
                    let p = try prompts.places.map {
                        FindPlaceVector(term: $0, vector: try encoder.textEmbedding(PhotoSearch.prompt($0)))
                    }
                    return (t, p)
                }.value
            }
            let r = try await backend.find(text, today: day, model: model, textVector: textVector,
                                           placeVectors: places, options: options)
            guard mine == ticket else { return }
            visualUnavailable = encoder == nil && (prompts.text != nil || !prompts.places.isEmpty)
            results = r
            phase = .results
        } catch is CancellationError {
            if mine == ticket { phase = .idle }
        } catch {
            if mine == ticket { phase = .failed(PeopleModel.message(error)) }
        }
    }

    /// The open session's latest ratings, people and faces reach the index first.
    private func syncOpenSession() async {
        guard let app, let session = app.currentSession, let people = app.people.backend else { return }
        // Best effort: an older copy of this session is still searchable.
        try? await people.syncFind(faceModel: app.people.modelID, suggest: app.people.tuning.autoMatch, in: session)
    }

    public func clear() {
        ticket += 1
        task?.cancel()
        results = nil
        visualUnavailable = false
        phase = .idle
    }

    // MARK: - Opening results

    /// Open a session from the results.
    public func open(_ session: FindSessionRecord) async {
        await openSession(URL(fileURLWithPath: session.root))
        dismiss()
    }

    /// Open a photo's session and show the photo.
    public func open(_ photo: FindPhotoRecord) async {
        guard let app else { return }
        await openSession(URL(fileURLWithPath: photo.sessionRoot))
        let target = URL(fileURLWithPath: photo.path)
        let name = target.lastPathComponent
        let candidates = app.photos.filter { $0.filename == name }.compactMap { p in p.url.map { (p.id, $0) } }
        // Symlinks touch the disk: off the main actor.
        let hit = await Task.detached(priority: .userInitiated) {
            let t = target.standardizedFileURL.resolvingSymlinksInPath()
            return candidates.first { $0.1.standardizedFileURL.resolvingSymlinksInPath() == t }?.0
        }.value
        if let hit {
            if !app.visibleIDs.contains(hit) { // hidden by the browser's filter or a search: show everything
                app.search.clear()
                app.filter = PhotoFilter()
            }
            app.focus(hit, extend: false)
        }
        dismiss()
    }

    private func openSession(_ root: URL) async {
        guard let app else { return }
        let current = app.currentSession?.url
        let same = await Task.detached(priority: .userInitiated) {
            current.map { $0.standardizedFileURL.resolvingSymlinksInPath() == root.standardizedFileURL.resolvingSymlinksInPath() }
                ?? false
        }.value
        if !same { await app.openFolder(root) }
    }

    // MARK: - Wording

    /// How the query was understood: one chip per part.
    public var summary: [Chip] {
        guard let q = results?.query else { return [] }
        var out: [Chip] = []
        for p in q.people where !p.names.isEmpty {
            out.append(Chip(kind: .person, text: Self.list(p.names)))
        }
        for p in q.places {
            out.append(Self.placeChip(p))
        }
        if let d = q.date { out.append(Chip(kind: .date, text: d.phrase)) }
        if let r = Self.rating(min: q.minRating, max: q.maxRating) { out.append(Chip(kind: .rating, text: r)) }
        if let f = q.flag { out.append(Chip(kind: .flag, text: Self.flag(f))) }
        if !q.text.isEmpty { out.append(Chip(kind: .visual, text: "looks like: \(q.text)")) }
        return out
    }

    /// What couldn't be used, in plain words.
    public var notes: [String] {
        guard let q = results?.query else { return [] }
        var out: [String] = q.people.filter(\.names.isEmpty).map {
            "No one named “\($0.term.capitalized)” yet — name faces in People."
        }
        for p in q.places where p.sources.isEmpty {
            out.append("Nothing is called “\(p.term)”.")
        }
        if visualUnavailable {
            out.append("Download the search model (search field ▸ Download) to match descriptions and places by look.")
        }
        return out
    }

    static func list(_ names: [String]) -> String {
        names.count <= 1 ? names.first ?? "" : names.dropLast().joined(separator: ", ") + " or " + names.last!
    }

    static func placeChip(_ p: FindPlaceRecord) -> Chip {
        guard let s = p.sources.first(where: { $0 != .looksLike }) ?? p.sources.first else {
            return Chip(kind: .place, text: p.term)
        }
        return source(s, term: p.term)
    }

    /// Where a place matched, named honestly.
    static func source(_ s: PlaceSourceRecord, term: String) -> Chip {
        switch s {
        case .location(let v): Chip(kind: .place, text: "location “\(v)”")
        case .sessionName(let n): Chip(kind: .place, text: "matched session name “\(n)”")
        case .folder(let n): Chip(kind: .place, text: "matched folder name “\(n)”")
        case .keyword(let k): Chip(kind: .place, text: "keyword “\(k)”")
        case .looksLike: Chip(kind: .looksLike, text: "looks like: \(term)")
        }
    }

    static func rating(min: UInt8?, max: UInt8?) -> String? {
        switch (min, max) {
        case (nil, nil): nil
        case (_, 0?): "Unrated"
        case let (lo?, hi?) where lo == hi: String(repeating: "★", count: Int(lo))
        case let (lo?, _): String(repeating: "★", count: Int(lo)) + " or more"
        case let (nil, hi?): "up to " + String(repeating: "★", count: Int(hi))
        }
    }

    static func flag(_ f: PickFlag) -> String {
        switch f {
        case .picked: "Picked"
        case .rejected: "Rejected"
        case .unflagged: "Unflagged"
        }
    }

    /// Why a photo matched.
    public static func chips(_ photo: FindPhotoRecord, phrase: String?) -> [Chip] {
        photo.reasons.map { r in
            switch r {
            case .person(let name, let suggested):
                Chip(kind: suggested ? .suggested : .person, text: suggested ? "\(name)?" : name)
            case .place(let term, let s): source(s, term: term)
            case .date: Chip(kind: .date, text: phrase ?? "date")
            case .rating(let stars): Chip(kind: .rating, text: stars == 0 ? "Unrated" : String(repeating: "★", count: Int(stars)))
            case .flag(let pick): Chip(kind: .flag, text: flag(pick))
            case .visual(let score): Chip(kind: .visual, text: "looks like it (\(Int((score * 100).rounded())))")
            case .text(let term): Chip(kind: .text, text: "“\(term)” in folder or keywords")
            case .title(let text): Chip(kind: .text, text: "titled “\(text)”")
            }
        }
    }

    /// Why a session is listed.
    public static func reason(_ s: FindSessionRecord) -> String {
        let photos = s.hits == 1 ? "1 photo" : "\(s.hits) photos"
        let why = switch s.reason {
        case .name(let term): "name matches “\(term)”"
        case .folder(_, let folder): "folder “\(folder)”"
        case .keyword(_, let keyword): "keyword “\(keyword)”"
        case .manyHits: "many matches"
        }
        return "\(why) · \(photos)"
    }
}
