import CoreGraphics
import CoreML
import CoreVideo
import CryptoKit
import LuxtableAI
import Foundation
import Observation
import Vision

/// A depth model (seam for tests: the real one is `CoreMLDepthEstimator`).
public protocol DepthEstimator: Sendable {
    /// Identity of the model: part of the cache key, so another model never reuses a map.
    var id: String { get }
    func estimate(_ image: CGImage) throws -> DepthField
}

/// The lens blur's depth map of one photo: the 8-bit E-frame map (255 = nearest) and the
/// subject matte it was refined with (the pipeline's auto focus reads it).
public struct DepthResult: Sendable, Equatable {
    public let depth: VisionMask
    public let subject: VisionMask?
    /// What made it: the model's id, or `DepthMapService.fallbackMethod`.
    public let method: String
}

/// Depth maps for the lens blur, one per photo: computed off the main actor, two at a time (the depth
/// model on the photo's analysis image, refined with the Vision subject matte; without the
/// model, the matte and a distance falloff), kept in memory and in `<support>/Cache/Depth`
/// (never next to the originals).
///
/// The map is in the **E frame** (oriented, uncropped, like the AI masks): crop, straighten
/// and quarter turns don't change it, so they never invalidate it. The cache key is the
/// photo's file identity (path, size, modification date: a rewritten file is a new photo)
/// and the method (model id with its revision, or the fallback): installing the model makes
/// new maps instead of reusing the fallback ones.
@MainActor @Observable
public final class DepthMapService {
    public static let shared = DepthMapService(model: .standard(.depthAnything))
    public nonisolated static let fallbackMethod = "fallback-1"
    static let memoryLimit = 8

    public let model: DownloadableModel
    /// Maps made so far (tests, diagnostics).
    public private(set) var computed = 0
    /// Called when a map was made for a photo that didn't have one yet.
    @ObservationIgnored public var onDepthReady: @MainActor (PhotoID) -> Void = { _ in }
    /// The subject matte (Vision person segmentation, else the salient subject).
    @ObservationIgnored var matte: @Sendable (CGImage) async -> VisionMask?
    /// The depth model when it's ready (nil: the fallback).
    @ObservationIgnored var estimator: @MainActor () async -> (any DepthEstimator)?
    @ObservationIgnored let cacheDirectory: URL
    @ObservationIgnored private var memory: [String: DepthResult] = [:]
    @ObservationIgnored private var order: [String] = []
    @ObservationIgnored private var jobs: [String: Task<DepthResult?, Never>] = [:]
    /// The photo each running job is for.
    @ObservationIgnored private var jobPhotos: [String: PhotoID] = [:]
    /// Who wants each running job's map.
    @ObservationIgnored private var interests: [String: JobInterest] = [:]
    /// At most this many maps are made at once (each holds an image and the model's buffers).
    static let concurrentJobs = 2
    @ObservationIgnored let slots = JobSlots(limit: DepthMapService.concurrentJobs)
    @ObservationIgnored private var loading: Task<Void, Never>?

    init(model: DownloadableModel, cacheDirectory: URL = DepthMapService.defaultCacheDirectory,
         matte: (@Sendable (CGImage) async -> VisionMask?)? = nil,
         estimator: (@MainActor () async -> (any DepthEstimator)?)? = nil) {
        self.model = model
        self.cacheDirectory = cacheDirectory
        self.matte = matte ?? { image in
            let v = VisionService()
            if let p = try? await v.personMask(image), p.coverage > 0.002 { return p }
            if let s = try? await v.subjectMask(image), s.coverage > 0.002 { return s }
            return nil
        }
        self.estimator = estimator ?? { nil }
        if estimator == nil {
            self.estimator = { [weak self] in await self?.loadedEstimator() }
        }
    }

    public nonisolated static var defaultCacheDirectory: URL {
        AppPaths.support.appendingPathComponent("Cache/Depth", isDirectory: true)
    }

    /// Cache key of a photo's map made by `method` (hex SHA-256; nil without a file).
    nonisolated static func key(for url: URL?, method: String) -> String? {
        guard let url, let attrs = try? FileManager.default.attributesOfItem(atPath: url.path) else { return nil }
        let size = (attrs[.size] as? NSNumber)?.int64Value ?? 0
        let mtime = (attrs[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0
        let text = "\(url.standardizedFileURL.path)|\(size)|\(mtime)|\(method)"
        return SHA256.hash(data: Data(text.utf8)).map { String(format: "%02x", $0) }.joined()
    }

    /// The photo's depth map: from memory or disk at once (the file is read off the main
    /// actor); otherwise computed from `image()` (the analysis image). With `wait` false
    /// (viewer, thumbnails) a missing map is started in the background and nil returned;
    /// `onDepthReady` fires when it lands. With `wait` (export) the call waits for it;
    /// cancelling the caller cancels the work unless someone else still wants the map.
    /// `urgent` maps (anyone waiting, the photo on screen) are made before the grid's
    /// background ones.
    public func depth(for photo: Photo, wait: Bool, urgent: Bool? = nil,
                      image: @escaping @Sendable () async -> CGImage?) async -> DepthResult? {
        let urgent = urgent ?? wait
        let est = await estimator()
        let method = est?.id ?? Self.fallbackMethod
        let url = photo.url
        guard let key = await Task.detached(operation: { Self.key(for: url, method: method) }).value else { return nil }
        if let hit = memory[key] { return hit }
        var job = live(key)
        if job == nil {
            let file = cacheDirectory.appendingPathComponent(key + ".depth")
            if let disk = await Task.detached(operation: { Self.read(file, method: method) }).value {
                remember(disk, key: key)
                return disk
            }
            if let hit = memory[key] { return hit } // landed while the file was read
            job = live(key)
        }
        // Joining is atomic with a waiter giving up: a job that stopped meanwhile is
        // replaced by a fresh one instead of handing this caller nothing.
        var picked = job ?? start(key: key, photo: photo.id, estimator: est, method: method, image: image)
        if !picked.1.join(waiting: wait, urgent: urgent) {
            picked = start(key: key, photo: photo.id, estimator: est, method: method, image: image)
            _ = picked.1.join(waiting: wait, urgent: urgent)
        }
        let (task, interest) = picked
        guard wait else { return nil }
        return await withTaskCancellationHandler {
            await task.value
        } onCancel: {
            interest.waiterLeft()
        }
    }

    /// `photo` left the screen: stop its maps that only the grid wanted in the background
    /// (not one a caller waits for, or the photo on screen asked for). A stopped job's
    /// queue place is given up at once.
    public func leftScreen(_ photo: PhotoID) {
        var any = false
        for (key, id) in jobPhotos where id == photo {
            if interests[key]?.stopIfOnlyBackground() == true {
                forget(key)
                any = true
            }
        }
        if any { Task { await slots.purge() } }
    }

    /// The running job for `key` that still makes its map.
    private func live(_ key: String) -> (Task<DepthResult?, Never>, JobInterest)? {
        guard let job = jobs[key], let interest = interests[key], !interest.isStopped else { return nil }
        return (job, interest)
    }

    /// Stop computing `photo`'s maps (it left the screen).
    public func cancel(_ photo: Photo) {
        for (key, id) in jobPhotos where id == photo.id {
            interests[key]?.stop()
            forget(key)
        }
    }

    /// How many callers wait for `photo`'s maps (tests).
    func waiters(for photo: PhotoID) -> Int {
        jobPhotos.filter { $0.value == photo }.reduce(0) { $0 + (interests[$1.key]?.waiters ?? 0) }
    }

    private func forget(_ key: String) {
        jobs[key] = nil
        jobPhotos[key] = nil
        interests[key] = nil
    }

    private func start(key: String, photo: PhotoID, estimator: (any DepthEstimator)?, method: String,
                       image: @escaping @Sendable () async -> CGImage?) -> (Task<DepthResult?, Never>, JobInterest) {
        let matte = self.matte
        let file = cacheDirectory.appendingPathComponent(key + ".depth")
        let dir = cacheDirectory
        let slots = self.slots
        let interest = JobInterest()
        let job = Task<DepthResult?, Never>.detached(priority: .userInitiated) {
            // Urgent jobs take a free slot first; a job stopped while queued never starts.
            guard await slots.acquire(interest) else { return nil }
            let made = await Self.compute(image: image, matte: matte, estimator: estimator, method: method)
            await slots.release()
            guard let made else { return nil }
            let (result, keep) = made
            if keep {
                try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                try? Self.encode(result).write(to: file, options: .atomic)
                Self.prune(dir, cap: Self.diskCap)
            }
            return result
        }
        interest.attach(job)
        jobs[key] = job
        jobPhotos[key] = photo
        interests[key] = interest
        Task { [weak self] in
            let result = await job.value
            guard let self else { return }
            if self.jobs[key] == job { self.forget(key) } // not a newer job for the same map
            guard let result else { return }
            self.computed += 1
            self.remember(result, key: key)
            self.onDepthReady(photo)
        }
        return (job, interest)
    }

    /// One map, and whether to keep it on disk: not when the depth model failed on this
    /// photo (the map is then the fallback's; the model gets another try next launch).
    nonisolated static func compute(image: @Sendable () async -> CGImage?, matte: @Sendable (CGImage) async -> VisionMask?,
                                    estimator: (any DepthEstimator)?, method: String) async -> (DepthResult, Bool)? {
        guard !Task.isCancelled, let img = await image(), !Task.isCancelled else { return nil }
        let subject = await matte(img)
        guard !Task.isCancelled else { return nil }
        var field: DepthField?
        var failed = false
        if let estimator {
            do { field = try estimator.estimate(img) } catch { failed = true }
        }
        guard !Task.isCancelled else { return nil }
        let (w, h) = Self.mapSize(img.width, img.height)
        let depth = DepthComposer.compose(depth: field, matte: subject, width: w, height: h)
        return (DepthResult(depth: depth, subject: subject, method: failed ? Self.fallbackMethod : method), !failed)
    }

    /// The map's size: the analysis image's, at most 1024 px on the long edge.
    nonisolated static func mapSize(_ w: Int, _ h: Int) -> (Int, Int) {
        let long = max(w, h, 1)
        let k = min(1, 1024 / Double(long))
        return (max(1, Int((Double(w) * k).rounded())), max(1, Int((Double(h) * k).rounded())))
    }

    private func remember(_ r: DepthResult, key: String) {
        memory[key] = r
        order.removeAll { $0 == key }
        order.append(key)
        while order.count > Self.memoryLimit { memory[order.removeFirst()] = nil }
    }

    /// Wait for every running computation (tests).
    func settle() async {
        while let job = jobs.values.first {
            _ = await job.value
            await Task.yield()
        }
    }

    // MARK: The model

    nonisolated static func modelID(_ d: DownloadableModelDescriptor) -> String {
        "\(d.name)-\(d.revision.prefix(12))"
    }

    /// The Core ML estimator once the model is downloaded (never downloads by itself).
    private func loadedEstimator() async -> (any DepthEstimator)? {
        guard model.state == .ready, let m = try? await model.loadedModel() else { return nil }
        return CoreMLDepthEstimator(model: m, id: Self.modelID(model.descriptor))
    }

    /// Download the depth model (the Lens Blur panel's button); the photo on screen gets a
    /// new map when it's ready.
    public func download(then ready: @escaping @MainActor () -> Void = {}) {
        guard loading == nil else { return }
        loading = Task { [weak self] in
            guard let self else { return }
            defer { self.loading = nil }
            if (try? await self.model.prepare()) != nil { ready() }
        }
    }

    /// What the Lens Blur panel says about depth.
    public var status: String {
        switch model.state {
        case .ready: "Depth from the AI depth model on this Mac."
        case .notDownloaded: "Depth from the subject only. Download the 50 MB depth model for real depth."
        case .downloading(let f): "Downloading the depth model… \(Int((f * 100).rounded())) %"
        case .failed(let m): m
        }
    }

    // MARK: Cache files

    /// `DSDP` 1, then the depth (w, h, bytes) and the subject (w, h, bytes; 0 × 0 = none).
    nonisolated static func encode(_ r: DepthResult) -> Data {
        var d = Data("DSDP".utf8)
        func u32(_ v: Int) { withUnsafeBytes(of: UInt32(v).littleEndian) { d.append(contentsOf: $0) } }
        u32(1)
        u32(r.depth.width)
        u32(r.depth.height)
        d.append(contentsOf: r.depth.bytes)
        u32(r.subject?.width ?? 0)
        u32(r.subject?.height ?? 0)
        if let s = r.subject { d.append(contentsOf: s.bytes) }
        return d
    }

    nonisolated static func decode(_ data: Data, method: String) -> DepthResult? {
        let b = [UInt8](data)
        var at = 4
        guard b.count >= 12, Array(b[0..<4]) == Array("DSDP".utf8) else { return nil }
        func u32() -> Int? {
            guard at + 4 <= b.count else { return nil }
            defer { at += 4 }
            return Int(UInt32(b[at]) | UInt32(b[at + 1]) << 8 | UInt32(b[at + 2]) << 16 | UInt32(b[at + 3]) << 24)
        }
        func bytes(_ n: Int) -> [UInt8]? {
            guard n >= 0, at + n <= b.count else { return nil }
            defer { at += n }
            return Array(b[at..<(at + n)])
        }
        guard u32() == 1, let w = u32(), let h = u32(), w > 0, h > 0, let depth = bytes(w * h),
              let sw = u32(), let sh = u32(), let sb = bytes(sw * sh), at == b.count else { return nil }
        let subject = sw > 0 && sh > 0 ? VisionMask(width: sw, height: sh, bytes: sb) : nil
        return DepthResult(depth: VisionMask(width: w, height: h, bytes: depth), subject: subject, method: method)
    }

    /// Reading a map marks it recently used (the disk cache drops the least recent).
    nonisolated static func read(_ url: URL, method: String) -> DepthResult? {
        guard let r = (try? Data(contentsOf: url)).flatMap({ decode($0, method: method) }) else { return nil }
        try? FileManager.default.setAttributes([.modificationDate: Date()], ofItemAtPath: url.path)
        return r
    }

    /// The disk cache's size limit (a map with its matte is about 2 MB).
    nonisolated static let diskCap: Int64 = 512 << 20

    /// Delete the least recently used maps in `dir` until they fit in `cap` bytes.
    nonisolated static func prune(_ dir: URL, cap: Int64) {
        let keys: [URLResourceKey] = [.fileSizeKey, .contentModificationDateKey]
        guard let files = try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: keys) else { return }
        var maps = files.filter { $0.pathExtension == "depth" }.map { url in
            let v = try? url.resourceValues(forKeys: Set(keys))
            return (url: url, size: Int64(v?.fileSize ?? 0), used: v?.contentModificationDate ?? .distantPast)
        }
        var total = maps.reduce(0) { $0 + $1.size }
        guard total > cap else { return }
        maps.sort { $0.used < $1.used }
        for m in maps where total > cap {
            try? FileManager.default.removeItem(at: m.url)
            total -= m.size
        }
    }
}

/// Who wants one depth job's map: callers waiting for it and renders that asked for it in
/// the background. A waiter that gives up stops the job only when it was the last one
/// that wanted it. Its cancellation handler runs off the main actor, so deciding and
/// cancelling happen under one lock, and a caller joining a job that just stopped is
/// refused (it starts a fresh one).
final class JobInterest: @unchecked Sendable {
    private let lock = NSLock()
    private var job: Task<DepthResult?, Never>?
    private var waiting = 0
    private var background = false
    private var urgent = false
    private var stopped = false

    init(_ job: Task<DepthResult?, Never>? = nil) {
        self.job = job
    }

    /// The job this is for (set right after it's made, before anyone can stop it).
    func attach(_ job: Task<DepthResult?, Never>) { lock.withLock { self.job = job } }

    var waiters: Int { lock.withLock { waiting } }
    var isStopped: Bool { lock.withLock { stopped } }
    /// Someone waits for the map, or the photo on screen asked for it: it goes first.
    var isUrgent: Bool { lock.withLock { urgent } }

    /// Want the map (`waiting`: a caller that waits for it); false when the job stopped.
    func join(waiting wait: Bool, urgent u: Bool = false) -> Bool {
        lock.withLock {
            guard !stopped else { return false }
            if wait { waiting += 1 } else { background = true }
            urgent = urgent || u || wait
            return true
        }
    }

    /// A waiter gave up: the job stops when nobody wants the map any more.
    func waiterLeft() {
        lock.withLock {
            waiting -= 1
            if waiting <= 0, !background { stopLocked() }
        }
    }

    func stop() { lock.withLock { stopLocked() } }

    /// Stop it if only the grid wanted it in the background; true when stopped.
    func stopIfOnlyBackground() -> Bool {
        lock.withLock {
            guard waiting <= 0, !urgent, !stopped else { return false }
            stopLocked()
            return true
        }
    }

    private func stopLocked() {
        stopped = true
        job?.cancel()
    }
}

/// The depth jobs' slots: at most `limit` run at once. A free slot goes to the oldest
/// urgent job, else the oldest background one; a job stopped while queued is dropped
/// (its `acquire` returns false) instead of waiting its turn.
actor JobSlots {
    let limit: Int
    private(set) var busy = 0
    private var waiting: [(interest: JobInterest, wake: CheckedContinuation<Bool, Never>)] = []

    init(limit: Int) {
        self.limit = limit
    }

    var queued: Int { waiting.count }

    /// Wait for a slot; false when the job stopped meanwhile (it then holds none).
    func acquire(_ interest: JobInterest) async -> Bool {
        if interest.isStopped { return false }
        guard busy >= limit else {
            busy += 1
            return true
        }
        return await withCheckedContinuation { waiting.append((interest, $0)) }
    }

    /// Hands the slot to the next job, if any.
    func release() {
        purge()
        if let i = waiting.firstIndex(where: { $0.interest.isUrgent }) ?? (waiting.isEmpty ? nil : 0) {
            waiting.remove(at: i).wake.resume(returning: true)
        } else {
            busy -= 1
        }
    }

    /// Drop the stopped jobs from the queue.
    func purge() {
        waiting.removeAll { w in
            guard w.interest.isStopped else { return false }
            w.wake.resume(returning: false)
            return true
        }
    }
}

/// Depth Anything V2 on Core ML: the image stretched to the model's input, the output read
/// as a float field (half, float or 8-bit one-component buffers).
struct CoreMLDepthEstimator: DepthEstimator, @unchecked Sendable {
    let model: MLModel
    let id: String

    func estimate(_ image: CGImage) throws -> DepthField {
        guard let input = model.modelDescription.inputDescriptionsByName["image"]?.imageConstraint else {
            throw ModelDownloadError.compile("depth model", "no image input")
        }
        let value = try MLFeatureValue(cgImage: image, constraint: input,
                                       options: [.cropAndScale: VNImageCropAndScaleOption.scaleFill.rawValue])
        let out = try model.prediction(from: MLDictionaryFeatureProvider(dictionary: ["image": value]))
        guard let buffer = out.featureValue(for: "depth")?.imageBufferValue, let field = Self.field(buffer) else {
            throw ModelDownloadError.compile("depth model", "no depth output")
        }
        return field
    }

    static func field(_ pb: CVPixelBuffer) -> DepthField? {
        CVPixelBufferLockBaseAddress(pb, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pb) else { return nil }
        let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), row = CVPixelBufferGetBytesPerRow(pb)
        var v = [Float](repeating: 0, count: w * h)
        switch CVPixelBufferGetPixelFormatType(pb) {
        case kCVPixelFormatType_OneComponent16Half:
            for y in 0..<h {
                let r = (base + y * row).assumingMemoryBound(to: UInt16.self)
                for x in 0..<w { v[y * w + x] = Float(Float16(bitPattern: r[x])) }
            }
        case kCVPixelFormatType_OneComponent32Float:
            for y in 0..<h {
                let r = (base + y * row).assumingMemoryBound(to: Float.self)
                for x in 0..<w { v[y * w + x] = r[x] }
            }
        case kCVPixelFormatType_OneComponent8:
            for y in 0..<h {
                let r = (base + y * row).assumingMemoryBound(to: UInt8.self)
                for x in 0..<w { v[y * w + x] = Float(r[x]) / 255 }
            }
        default:
            return nil
        }
        return DepthField(width: w, height: h, values: v)
    }
}
