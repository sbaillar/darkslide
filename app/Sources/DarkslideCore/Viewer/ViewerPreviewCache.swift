import CoreGraphics
import Foundation

/// Embedded-preview frames for instant navigation, with prefetch.
///
/// Previews come from `PreviewProvider` (decoded off the main thread) and are
/// converted to 8-bit frames off the main thread too, so showing the next image
/// is a dictionary lookup when prefetch got there first.
@MainActor
final class ViewerPreviewCache {
    struct Entry {
        let frame: RenderedFrame
        let image: CGImage
    }

    private let provider: any PreviewProvider
    private let frames: PreviewFrames
    /// Bytes of preview frames kept (bounded by bytes, not count).
    let byteLimit: Int
    private(set) var bytes = 0
    private var entries: [PhotoID: Entry] = [:]
    private var order: [PhotoID] = []
    private var tasks: [PhotoID: (token: Int, task: Task<Void, Never>)] = [:]
    private var nextToken = 0
    /// Callbacks waiting for an in-flight load (a prefetch the user navigated onto).
    private var waiters: [PhotoID: [@MainActor (Entry?) -> Void]] = [:]
    /// Long edge requested from the provider (0 = the full embedded preview).
    let maxPixelSize: Int
    private(set) var loads = 0

    init(provider: any PreviewProvider, byteLimit: Int = 96 << 20, maxPixelSize: Int = 0,
         frames: PreviewFrames = .shared) {
        self.provider = provider
        self.byteLimit = byteLimit
        self.maxPixelSize = maxPixelSize
        self.frames = frames
    }

    func entry(_ id: PhotoID) -> Entry? { entries[id] }
    var isLoadingAnything: Bool { !tasks.isEmpty }
    func isLoading(_ id: PhotoID) -> Bool { tasks[id] != nil }

    /// Start loading `photo` unless cached or in flight. `done` runs on the main actor.
    func load(_ photo: Photo, priority: TaskPriority = .userInitiated, done: @escaping @MainActor (Entry?) -> Void) {
        if let e = entries[photo.id] {
            touch(photo.id)
            done(e)
            return
        }
        guard let url = photo.url else { return }
        waiters[photo.id, default: []].append(done)
        guard tasks[photo.id] == nil else { return }
        loads += 1
        nextToken += 1
        let provider = provider, size = maxPixelSize, id = photo.id, frames = frames, token = nextToken
        let task = Task(priority: priority) { [weak self] in
            // The decode runs detached (off the main actor), which doesn't inherit this
            // task's cancellation: forward it, so a cancelled prefetch stops decoding.
            let decode = Task.detached(priority: priority) { () -> Entry? in
                guard !Task.isCancelled, let img = try? await provider.preview(for: url, maxPixelSize: size),
                      !Task.isCancelled, let frame = frames.frame(for: img) else { return nil }
                return Entry(frame: frame, image: img)
            }
            let entry = await withTaskCancellationHandler { await decode.value } onCancel: { decode.cancel() }
            // A cancelled load was already unregistered (and its waiters told) by `cancel`;
            // a newer load of the same photo owns the slot now: leave both alone.
            guard let self, self.tasks[id]?.token == token else { return }
            self.tasks[id] = nil
            if let entry { self.store(id, entry) }
            for w in self.waiters.removeValue(forKey: id) ?? [] { w(entry) }
        }
        tasks[id] = (token, task)
    }

    /// Cancel in-flight loads except for `keep`.
    func cancel(except keep: Set<PhotoID>) {
        for (id, t) in tasks where !keep.contains(id) {
            t.task.cancel()
            tasks[id] = nil
            for w in waiters.removeValue(forKey: id) ?? [] { w(nil) }
        }
    }

    /// Drop every frame and stop every load (another session opened: ids repeat across sessions).
    func removeAll() {
        cancel(except: [])
        entries = [:]
        order = []
        bytes = 0
    }

    private func store(_ id: PhotoID, _ e: Entry) {
        if let old = entries[id] { bytes -= old.frame.byteCount }
        entries[id] = e
        bytes += e.frame.byteCount
        touch(id)
        while bytes > byteLimit, order.count > 1 {
            if let gone = entries.removeValue(forKey: order.removeFirst()) { bytes -= gone.frame.byteCount }
        }
    }

    private func touch(_ id: PhotoID) {
        order.removeAll { $0 == id }
        order.append(id)
    }
}
