import Foundation
import Observation

/// Debounced, retrying persistence of culling metadata and edits (M6/M7).
///
/// A slider drag produces dozens of changes; they collapse into one write after
/// `debounce`. A failed write keeps its changes pending (newer changes merge on
/// top), and `state` stays `.failed` until a retry succeeds, so the UI can show
/// a persistent "not saved" indicator instead of a transient toast.
@MainActor @Observable
public final class SaveCoordinator {
    public enum State: Equatable, Sendable {
        case saved
        case pending(Int)
        case saving
        case failed(String, pending: Int)
        /// Changes the store said can never be written (their session is closed) were
        /// dropped: nothing to retry, but "not saved" stays shown until dismissed.
        case dropped(String, count: Int)
    }

    public private(set) var state: State = .saved
    @ObservationIgnored private var photos: [PhotoID: Photo] = [:]
    @ObservationIgnored private var edits: [PhotoID: EditState] = [:]
    @ObservationIgnored private let store: any SessionStore
    @ObservationIgnored private let debounce: Duration
    @ObservationIgnored private var timer: Task<Void, Never>?
    @ObservationIgnored private var tail: Task<Void, Never>?
    public private(set) var writes = 0
    /// Changes the store said can never be written (their session is closed): dropped,
    /// not retried, so they can't hold up everything else. Counted until dismissed.
    public private(set) var dropped = 0
    public private(set) var lastDropMessage: String?
    /// Told about each drop (the app shows a toast).
    @ObservationIgnored public var onDrop: (@MainActor (String) -> Void)?

    public init(store: any SessionStore, debounce: Duration = .milliseconds(400)) {
        self.store = store
        self.debounce = debounce
    }

    public var pendingCount: Int { Set(photos.keys).union(edits.keys).count }
    public var hasUnsaved: Bool { pendingCount > 0 || state == .saving }

    public func enqueue(photos changed: [Photo]) {
        for p in changed { photos[p.id] = p }
        schedule()
    }

    public func enqueue(edits changed: [PhotoID: EditState]) {
        edits.merge(changed) { _, new in new }
        schedule()
    }

    private func schedule() {
        if case .failed(let m, _) = state {
            state = .failed(m, pending: pendingCount)
        } else {
            state = .pending(pendingCount)
        }
        timer?.cancel()
        let delay = debounce
        timer = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            await self?.flush()
        }
    }

    /// Write everything pending now (drag end, session switch, quit, retry).
    /// Calls are serialized (L4): each flush runs after the previous one, so
    /// writes land in order and `.saved` is only set when nothing is running.
    public func flush() async {
        timer?.cancel()
        let previous = tail
        let task = Task { [weak self] in
            await previous?.value
            await self?.writePending()
        }
        tail = task
        await task.value
    }

    private func writePending() async {
        guard pendingCount > 0 else {
            if case .failed = state {} else { state = settled }
            return
        }
        let (p, e) = (photos, edits)
        photos = [:]
        edits = [:]
        state = .saving
        var failure: Error?
        var retryPhotos: [PhotoID: Photo] = [:]
        var retryEdits: [PhotoID: EditState] = [:]
        if !p.isEmpty {
            do { try await store.save(photos: Array(p.values)) } catch let u as UnsavableChanges { drop(u) } catch {
                failure = error
                retryPhotos = p
            }
        }
        if !e.isEmpty {
            do { try await store.save(edits: e) } catch let u as UnsavableChanges { drop(u) } catch {
                failure = error
                retryEdits = e
            }
        }
        if let failure { didFail(failure, photos: retryPhotos, edits: retryEdits) } else { didSave() }
    }

    /// The rest of that batch was written; these never can be.
    private func drop(_ u: UnsavableChanges) {
        dropped += u.ids.count
        lastDropMessage = u.localizedDescription
        onDrop?(u.localizedDescription)
        AppLog.session.error("dropped \(u.ids.count, privacy: .public) unsavable change(s): \(u.localizedDescription, privacy: .public)")
    }

    private func didSave() {
        writes += 1
        state = pendingCount > 0 ? .pending(pendingCount) : settled
    }

    /// Nothing pending: saved, or still saying that some changes were dropped.
    private var settled: State {
        guard dropped > 0, let m = lastDropMessage else { return .saved }
        return .dropped(m, count: dropped)
    }

    /// The user read the "not saved" notice.
    public func dismissDropped() {
        dropped = 0
        lastDropMessage = nil
        if case .dropped = state { state = .saved }
    }

    private func didFail(_ error: Error, photos p: [PhotoID: Photo], edits e: [PhotoID: EditState]) {
        // Newer changes made during the write win over the failed ones.
        photos.merge(p) { newer, _ in newer }
        edits.merge(e) { newer, _ in newer }
        state = .failed(error.localizedDescription, pending: pendingCount)
    }

    public func retry() async { await flush() }
}

/// A batch was written except these photos, whose session is closed (or which the store
/// never had): a late write that must fail loudly, never land elsewhere or be retried.
public struct UnsavableChanges: Error, LocalizedError, Equatable {
    public let ids: Set<PhotoID>
    public init(ids: Set<PhotoID>) { self.ids = ids }
    public var errorDescription: String? {
        "\(ids.count) change\(ids.count == 1 ? "" : "s") couldn’t be saved: the session \(ids.count == 1 ? "it belongs" : "they belong") to is closed."
    }
}
