import LuxtableFFI
import Foundation

/// Engine progress as an `AsyncStream`.
///
/// Install with `engine.setProgressListener(listener: stream)` and consume
/// `events` from one task (e.g. on the main actor). Rust delivers on its own
/// delivery thread and never waits for Swift. This side keeps at most
/// `limit` undelivered `Running` events (the oldest are dropped first, so a
/// slow consumer sees fresh progress, not a backlog), while `Started` and the
/// terminal `Finished` / `Cancelled` / `Failed` events are **never** dropped
/// (M11): every task visibly ends. No `Task` is spawned per event.
///
/// Lifetime: the engine holds this object strongly; it holds no reference to
/// its consumer, so there is no cycle. Call `finish()` (or remove the
/// listener) when done so `for await` loops end.
public final class ProgressStream: ProgressListener, @unchecked Sendable {
    public let events: AsyncStream<ProgressEvent>
    private let buffer: ProgressBuffer

    public init(bufferingNewest limit: Int = 32) {
        let buffer = ProgressBuffer(limit: limit)
        self.buffer = buffer
        events = AsyncStream(unfolding: { await buffer.next() })
    }

    public func onProgress(event: ProgressEvent) {
        buffer.push(event)
    }

    /// Events dropped so far (only ever `Running` ones).
    public var dropped: Int { buffer.dropped }

    public func finish() {
        buffer.finish()
    }

    deinit { buffer.finish() }
}

/// Lock-protected queue behind `ProgressStream` (one consumer).
final class ProgressBuffer: @unchecked Sendable {
    private let lock = NSLock()
    private var queue: [ProgressEvent] = []
    private var waiter: CheckedContinuation<ProgressEvent?, Never>?
    private var finished = false
    private var runningQueued = 0
    private var droppedCount = 0
    let limit: Int

    init(limit: Int) { self.limit = max(1, limit) }

    var dropped: Int { lock.withLock { droppedCount } }

    func push(_ e: ProgressEvent) {
        let resume: CheckedContinuation<ProgressEvent?, Never>? = lock.withLock {
            guard !finished else { return nil }
            if let w = waiter {
                waiter = nil
                return w
            }
            queue.append(e)
            if e.phase == .running {
                runningQueued += 1
                if runningQueued > limit, let i = queue.firstIndex(where: { $0.phase == .running }) {
                    queue.remove(at: i)
                    runningQueued -= 1
                    droppedCount += 1
                }
            }
            return nil
        }
        resume?.resume(returning: e)
    }

    func finish() {
        let w: CheckedContinuation<ProgressEvent?, Never>? = lock.withLock {
            finished = true
            defer { waiter = nil }
            return queue.isEmpty ? waiter : nil
        }
        w?.resume(returning: nil)
    }

    /// Next event; `nil` once finished and drained (or the consumer was cancelled).
    func next() async -> ProgressEvent? {
        await withTaskCancellationHandler {
            await withCheckedContinuation { (c: CheckedContinuation<ProgressEvent?, Never>) in
                let immediate: ProgressEvent?? = lock.withLock {
                    if !queue.isEmpty {
                        let e = queue.removeFirst()
                        if e.phase == .running { runningQueued -= 1 }
                        return .some(e)
                    }
                    if finished || Task.isCancelled { return .some(nil) }
                    waiter = c
                    return .none
                }
                if let immediate { c.resume(returning: immediate) }
            }
        } onCancel: {
            let w: CheckedContinuation<ProgressEvent?, Never>? = lock.withLock {
                defer { waiter = nil }
                return waiter
            }
            w?.resume(returning: nil)
        }
    }
}
