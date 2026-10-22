import CoreML
import Foundation
import Observation

/// The natural-language search model: Apple's MobileCLIP-S2 image and text
/// encoders (Core ML, converted by Apple) plus CLIP's BPE merges for the
/// tokenizer, downloaded on first use.
///
/// - Image encoder: 256×256 RGB in, a 512-d embedding out (`final_emb_1`).
/// - Text encoder: 77 int32 token ids in, a 512-d embedding out.
/// - License: the Core ML weights (huggingface.co/apple/coreml-mobileclip, pinned
///   below) are under Apple's sample-code license for the MobileCLIP weights
///   (`apple-ascl`, LICENSE_weights_data in apple/ml-mobileclip at 341ef05):
///   use, reproduction and redistribution allowed with the notice kept. The
///   merges file is OpenAI CLIP's (MIT), from openai/clip-vit-base-patch32.
///
/// Files are pinned by commit (never `resolve/main`) and SHA-256, like the sky model.
public struct ClipModelDescriptor: Sendable, Equatable {
    public struct File: Sendable, Equatable {
        /// Path inside the model folder (`image.mlpackage/Manifest.json`, `merges.txt`).
        public var path: String
        public var url: URL
        public var sha256: String
        public var size: Int64
    }

    /// Model id stored with every embedding (vectors of different models never mix).
    public var id: String
    public var name: String
    public var files: [File]
    /// Embedding length.
    public var dimensions: Int

    public var totalBytes: Int64 { files.reduce(0) { $0 + $1.size } }

    /// Commit of huggingface.co/apple/coreml-mobileclip (its head on 2024-11-19).
    public static let mobileCLIPRevision = "3e0a7bfb9fe83da8a3efaa3fd8f7df24214bb947"
    /// Commit of huggingface.co/openai/clip-vit-base-patch32 the merges come from.
    public static let mergesRevision = "3d74acf9a28c67741b2f4f2ea7635f0aaf6f0268"

    public static let mobileCLIPS2: ClipModelDescriptor = {
        let hf = "https://huggingface.co/"
        func pkg(_ local: String, _ remote: String, _ sha: [String], _ sizes: [Int64]) -> [File] {
            let parts = ["Manifest.json", "Data/com.apple.CoreML/model.mlmodel", "Data/com.apple.CoreML/weights/weight.bin"]
            return parts.indices.map { i in
                File(path: "\(local).mlpackage/\(parts[i])",
                     url: URL(string: hf + "apple/coreml-mobileclip/resolve/\(mobileCLIPRevision)/\(remote).mlpackage/\(parts[i])")!,
                     sha256: sha[i], size: sizes[i])
            }
        }
        let image = pkg("image", "mobileclip_s2_image", [
            "6a1a3f93b8dca6c237dbb5dc7b19bb3c987042d14860288304986c099d8796b6",
            "2aeb3359f6cde65e9f9248ec2a742e9939bd4bbf48c2f55fcd255b4504d96a1b",
            "6cbc7fb06b6072c1cae9c4496d67e0e6217adbf726dfeb82e44d4efe87c34c00",
        ], [617, 299_056, 71_397_632])
        let text = pkg("text", "mobileclip_s2_text", [
            "ea6a189d82fee2ee36eadb99022cda66a7f6b50a4bf9cc1d9541c7ea3b4242a0",
            "b8651b6d030bae419a9548b41c8fae11f96b59cfa21b6e532a4c4434522b4b80",
            "8e8d5454f104b6cbb58d98bf11e038ff1f1943599efea111260a832f094cd0ce",
        ], [617, 128_127, 126_866_880])
        let merges = File(path: "merges.txt",
                          url: URL(string: hf + "openai/clip-vit-base-patch32/resolve/\(mergesRevision)/merges.txt")!,
                          sha256: "f526393189112391ce6f9795d4695f704121ce452c3aad1f5335cc41337eba85", size: 524_657)
        return ClipModelDescriptor(id: "mobileclip-s2", name: "MobileCLIP-S2", files: image + text + [merges], dimensions: 512)
    }()
}

/// The installed model: compiled encoders and the merges file.
public struct ClipModelFiles: Sendable, Equatable {
    public var imageModel: URL
    public var textModel: URL
    public var merges: URL

    init(root: URL) {
        imageModel = root.appendingPathComponent("image.mlmodelc", isDirectory: true)
        textModel = root.appendingPathComponent("text.mlmodelc", isDirectory: true)
        merges = root.appendingPathComponent("merges.txt")
    }

    var all: [URL] { [imageModel, textModel, merges] }
}

public enum ClipModelError: Error, Equatable, LocalizedError {
    case download(String)
    case checksum(String)
    case compile(String)
    case load(String)

    public var errorDescription: String? {
        switch self {
        case .download(let m): "Couldn't download the search model: \(m)"
        case .checksum(let f): "The downloaded search model is damaged (\(f)). Try again."
        case .compile(let m): "Couldn't prepare the search model: \(m)"
        case .load(let m): "Couldn't load the search model: \(m)"
        }
    }
}

/// "Downloads" from a local copy of the files: the URL's path after
/// `/resolve/<commit>/` under `root`, so one folder holding both `.mlpackage`s
/// and `merges.txt` stands in for the network (`DS_CLIP_MODEL_DIR`).
public struct DirectoryModelDownloader: ModelDownloader {
    public let root: URL
    public init(root: URL) { self.root = root }

    public func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
        let parts = url.path.split(separator: "/", omittingEmptySubsequences: true)
        guard let r = parts.firstIndex(of: "resolve"), r + 2 < parts.count else {
            throw ClipModelError.download(url.lastPathComponent)
        }
        let src = root.appendingPathComponent(parts[(r + 2)...].joined(separator: "/"))
        let fm = FileManager.default
        try? fm.removeItem(at: destination)
        do {
            try fm.copyItem(at: src, to: destination)
        } catch {
            throw ClipModelError.download(error.localizedDescription)
        }
        progress((try? fm.attributesOfItem(atPath: destination.path)[.size] as? NSNumber)?.int64Value ?? 0)
    }
}

/// The search model on this Mac: downloaded into the support folder on first
/// use (checksummed, with progress and cancel), both packages compiled with
/// `MLModel.compileModel(at:)`, then moved into place in one step, so a
/// half-finished download is never "ready".
@MainActor @Observable
public final class ClipModelStore {
    public enum State: Equatable, Sendable {
        case notDownloaded
        case downloading(Double)
        case ready
        case failed(String)
    }

    /// `<support>/Models/MobileCLIP-S2`. `DS_CLIP_MODEL_DIR` (a folder with the two
    /// `.mlpackage`s and `merges.txt`) replaces the network for development.
    public static let shared = ClipModelStore(
        root: AppPaths.support.appendingPathComponent("Models", isDirectory: true),
        downloader: ProcessInfo.processInfo.environment["DS_CLIP_MODEL_DIR"].map {
            DirectoryModelDownloader(root: URL(fileURLWithPath: $0))
        } ?? URLSessionModelDownloader())

    public private(set) var state: State
    public let descriptor: ClipModelDescriptor
    /// Where the installed model lives.
    public let folder: URL
    @ObservationIgnored private let root: URL
    @ObservationIgnored private let downloader: any ModelDownloader
    @ObservationIgnored private let compile: @Sendable (URL) async throws -> URL
    @ObservationIgnored private var preparing: Task<ClipModelFiles, Error>?
    @ObservationIgnored private var sweeping: Task<Void, Never>?

    public init(root: URL, descriptor: ClipModelDescriptor = .mobileCLIPS2,
                downloader: any ModelDownloader = URLSessionModelDownloader(),
                compile: @escaping @Sendable (URL) async throws -> URL = ModelFiles.compile) {
        self.root = root
        self.descriptor = descriptor
        self.downloader = downloader
        self.compile = compile
        folder = root.appendingPathComponent(descriptor.name, isDirectory: true)
        state = ClipModelFiles(root: folder).all.allSatisfy { FileManager.default.fileExists(atPath: $0.path) }
            ? .ready : .notDownloaded
    }

    public var files: ClipModelFiles { ClipModelFiles(root: folder) }

    public var isReady: Bool { state == .ready }

    /// The installed files, downloading and compiling them first if needed (one
    /// download at a time). `cancel()` makes this throw `CancellationError`, and
    /// the state goes back to `.notDownloaded`.
    public func prepare() async throws -> ClipModelFiles {
        // Once per launch, before any download of ours can have a staging folder.
        if sweeping == nil {
            let root = root
            sweeping = Task.detached(priority: .utility) {
                ModelFiles.sweepStaging(in: root, prefix: Self.stagingPrefix)
                ModelFiles.sweepLegacyStaging(in: root)
            }
        }
        await sweeping?.value
        if files.all.allSatisfy({ FileManager.default.fileExists(atPath: $0.path) }) {
            state = .ready
            return files
        }
        if let preparing { return try await preparing.value }
        let task = Task { try await self.install() }
        preparing = task
        defer { preparing = nil }
        do {
            let f = try await task.value
            state = .ready
            return f
        } catch {
            if error is CancellationError || task.isCancelled {
                state = .notDownloaded
                throw CancellationError()
            }
            state = .failed((error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
            throw error
        }
    }

    /// Stop the download in progress; nothing is kept.
    public func cancel() {
        preparing?.cancel()
    }

    /// Staging folders are `<root>/.clip-download-<uuid>` (up to ~200 MB each): the sweep of
    /// the ones a crash or force-quit left behind takes only this model's prefix, never
    /// another model's download in flight.
    nonisolated static let stagingPrefix = ".clip-download-"

    /// What `install` records: the SHA-256 of every installed file (relative path → hex).
    nonisolated static let manifestName = "installed.sha256.json"

    nonisolated static func digest(of folder: URL) throws -> [String: String] {
        let base = folder.standardizedFileURL.path + "/"
        var out: [String: String] = [:]
        guard let walk = FileManager.default.enumerator(at: folder, includingPropertiesForKeys: [.isRegularFileKey]) else {
            return out
        }
        for case let url as URL in walk {
            guard try url.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile == true else { continue }
            let rel = String(url.standardizedFileURL.path.dropFirst(base.count))
            if rel != manifestName { out[rel] = try ModelFiles.sha256(of: url) }
        }
        return out
    }

    /// Whether the installed files are still what `install` wrote (a load failed: a
    /// transient one keeps them, a damaged or unrecorded install is fetched again).
    public func verify() async -> Bool {
        let folder = folder
        return await Task.detached(priority: .utility) {
            guard let data = try? Data(contentsOf: folder.appendingPathComponent(Self.manifestName)),
                  let recorded = try? JSONDecoder().decode([String: String].self, from: data),
                  !recorded.isEmpty else { return false }
            return (try? Self.digest(of: folder)) == recorded
        }.value
    }

    /// Forget the installed model (it is downloaded again on next use), e.g. after
    /// it failed to load.
    public func discard() {
        try? FileManager.default.removeItem(at: folder)
        state = .notDownloaded
    }

    private func install() async throws -> ClipModelFiles {
        let fm = FileManager.default
        let staging = root.appendingPathComponent(Self.stagingPrefix + UUID().uuidString, isDirectory: true)
        do {
            let f = try await install(into: staging)
            try? fm.removeItem(at: staging)
            return f
        } catch {
            try? fm.removeItem(at: staging)
            throw error
        }
    }

    /// Download and verify every file into `staging`, compile both packages
    /// there, then move the finished model into place at once.
    private func install(into staging: URL) async throws -> ClipModelFiles {
        let fm = FileManager.default
        try fm.createDirectory(at: staging, withIntermediateDirectories: true)
        let total = max(descriptor.totalBytes, 1)
        var done: Int64 = 0
        state = .downloading(0)
        for file in descriptor.files {
            try Task.checkCancellation()
            try await download(file, into: staging, total: total, before: done)
            done += file.size
            state = .downloading(Double(done) / Double(total))
        }
        let ready = staging.appendingPathComponent("model", isDirectory: true)
        try fm.createDirectory(at: ready, withIntermediateDirectories: true)
        let image = try await Self.compile(staging.appendingPathComponent("image.mlpackage", isDirectory: true), with: compile)
        try fm.moveItem(at: image, to: ready.appendingPathComponent("image.mlmodelc", isDirectory: true))
        try Task.checkCancellation()
        let text = try await Self.compile(staging.appendingPathComponent("text.mlpackage", isDirectory: true), with: compile)
        try fm.moveItem(at: text, to: ready.appendingPathComponent("text.mlmodelc", isDirectory: true))
        try fm.moveItem(at: staging.appendingPathComponent("merges.txt"), to: ready.appendingPathComponent("merges.txt"))
        let manifest = try await Task.detached(priority: .utility) { try JSONEncoder().encode(Self.digest(of: ready)) }.value
        try manifest.write(to: ready.appendingPathComponent(Self.manifestName), options: .atomic)
        try Task.checkCancellation()
        try? fm.removeItem(at: folder)
        try fm.moveItem(at: ready, to: folder)
        return files
    }

    private nonisolated static func compile(_ package: URL,
                                            with compile: @Sendable (URL) async throws -> URL) async throws -> URL {
        do {
            return try await compile(package)
        } catch {
            throw ClipModelError.compile(error.localizedDescription)
        }
    }

    private func download(_ file: ClipModelDescriptor.File, into staging: URL, total: Int64, before: Int64) async throws {
        let dst = staging.appendingPathComponent(file.path)
        try FileManager.default.createDirectory(at: dst.deletingLastPathComponent(), withIntermediateDirectories: true)
        let report = Self.reporter(total: total, before: before) { [weak self] f in
            if case .downloading = self?.state { self?.state = .downloading(f) }
        }
        do {
            try await ModelFiles.fetch(file.url, to: dst, sha256: file.sha256, size: file.size, path: file.path,
                                       using: downloader, progress: report)
        } catch let e as ModelFetchError {
            switch e {
            case .download(let m): throw ClipModelError.download(m)
            case .checksum(let f): throw ClipModelError.checksum(f)
            }
        }
    }

    /// Bytes of one file → the whole download's fraction, delivered on the main actor.
    private nonisolated static func reporter(total: Int64, before: Int64,
                                             _ update: @escaping @MainActor @Sendable (Double) -> Void) -> @Sendable (Int64) -> Void {
        { bytes in
            let f = min(1, Double(before + bytes) / Double(total))
            Task { @MainActor in update(f) }
        }
    }
}
