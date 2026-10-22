import CoreML
import Foundation
import Observation

/// A Core ML model fetched on first use: its files (path in the `.mlpackage`, pinned
/// URL, SHA-256, size), how to run it and where `DS_*_MODEL_DIR` may point instead.
public struct DownloadableModelDescriptor: Sendable, Equatable {
    public struct File: Sendable, Equatable {
        /// Path inside the `.mlpackage`.
        public var path: String
        public var url: URL
        /// Hex SHA-256; empty = not pinned (the caller checks the file's shape instead).
        public var sha256: String
        public var size: Int64
    }

    /// Package / compiled-model file name (`<name>.mlmodelc`).
    public var name: String
    /// Shown in messages ("the removal model").
    public var displayName: String
    public var files: [File]
    /// The compute units the conversion was verified with.
    public var computeUnits: MLComputeUnits
    /// Environment variable naming a local copy of the `.mlpackage` (development, tests).
    public var environmentKey: String
    /// The pinned upstream revision, part of the compiled copy's name: a new revision is
    /// never taken for the old one's compiled model.
    public var revision: String = ""
    /// Folder name an earlier build installed the same pinned files under; it is renamed to
    /// `compiledName` instead of being fetched again.
    public var legacyCompiledName: String?

    /// Staging folders are `<root>/.<name>-download-<uuid>`: a sweep of one model's never
    /// takes another model's download in flight.
    public var stagingPrefix: String { ".\(name)-download-" }

    /// The compiled model's folder name (`<name>-<revision prefix>.mlmodelc`).
    public var compiledName: String {
        (revision.isEmpty ? name : "\(name)-\(revision.prefix(12))") + ".mlmodelc"
    }

    public var totalBytes: Int64 { files.reduce(0) { $0 + $1.size } }

    static func file(_ base: String, _ path: String, _ sha: String, _ size: Int64) -> File {
        File(path: path, url: URL(string: base + path)!, sha256: sha, size: size)
    }

    /// LaMa (big-lama, advimman/lama, Apache-2.0) converted to Core ML by john-rocky:
    /// inputs `image` (colour 800×800) and `mask` (grayscale 800×800, 255 = fill),
    /// output `output` (colour 800×800). Converted for `.all`. Pinned by commit.
    /// big-lama was trained on Places365, whose terms are non-commercial research: it is
    /// downloaded on first use, never bundled (Acknowledgements).
    public static let lama: DownloadableModelDescriptor = {
        let base = "https://huggingface.co/mlboydaisuke/LaMa-CoreML/resolve/\(lamaRevision)/LaMa.mlpackage/"
        return DownloadableModelDescriptor(
            name: "LaMa", displayName: "removal model",
            files: [
                file(base, "Manifest.json", "c814fff3cedf827c044094545ef80b0280b6cb8dd0e5c0bcf69fd31921191e58", 617),
                file(base, "Data/com.apple.CoreML/model.mlmodel",
                     "06a100ef99e0fd16326a3a8c4a687d13f7b26f544ea906a75338932d8554f953", 1_101_809),
                file(base, "Data/com.apple.CoreML/weights/weight.bin",
                     "d0541f6044a94cd4982bfdac074fc1ccfe11d8f1f590c299d6b5071b501fc184", 215_544_960),
            ],
            computeUnits: .all, environmentKey: "DS_LAMA_MODEL_DIR", revision: lamaRevision)
    }()

    /// Apple's Core ML semantic segmentation (DETR-ResNet50, COCO panoptic, Float16):
    /// 448×448 input, per-pixel class ids; "sky (other)" is class 187 (`SkySegmentation`).
    ///
    /// Why not SegFormer ADE20K (sky = index 2): no converted Core ML SegFormer exists on
    /// Hugging Face, and converting PyTorch/ONNX needs coremltools + torch, which the app
    /// can't run. Apple's model is Apple-converted, Apache-2.0 and segments sky as one of
    /// its stuff classes. The Neural Engine (with the CPU) runs it in ~30 ms, against
    /// 65-73 ms when Core ML also considers the GPU.
    ///
    /// The files are fetched at a pinned commit (`skyRevision`), never `resolve/main`: a
    /// new upload to the repo can't turn every install into a checksum failure.
    public static let sky: DownloadableModelDescriptor = {
        let base = "https://huggingface.co/apple/coreml-detr-semantic-segmentation/resolve/\(skyRevision)/"
            + "DETRResnet50SemanticSegmentationF16.mlpackage/"
        return DownloadableModelDescriptor(
            name: "DETRResnet50SemanticSegmentationF16", displayName: "sky model",
            files: [
                file(base, "Manifest.json", "0749549c6b11139e11a40aaab82add4843a9977010337e596cce3dbb35c98512", 617),
                file(base, "Data/com.apple.CoreML/model.mlmodel",
                     "61e05678150bcae26deac589b998dffe13c2977a9ba6f802938a9f3a60bfdcce", 309_817),
                file(base, "Data/com.apple.CoreML/weights/weight.bin",
                     "2f4de3dfed1aeced35ad7096d7c8721fcfb59571110a9e9138d73a7e56480408", 85_158_176),
            ],
            computeUnits: .cpuAndNeuralEngine, environmentKey: "DS_SKY_MODEL_DIR", revision: skyRevision,
            legacyCompiledName: "DETRResnet50SemanticSegmentationF16.mlmodelc")
    }()

    /// Commit of huggingface.co/apple/coreml-detr-semantic-segmentation the sky model is
    /// pinned to (the repo's head on 2024-08-09; the checksums above are of its files).
    public static let skyRevision = "7c771f8867a479d1441ac5fb0a8de31feea76bb6"

    public static let lamaRevision = "5ed76e3799ab4cad31381750d29880c267477e18"

    /// BiSeNet face parsing (zllrunning/face-parsing.PyTorch, MIT weights) converted to
    /// Core ML by john-rocky: input `input` (colour 512×512), output `argmax_out`
    /// (per-pixel class ids of the 19 CelebAMask-HQ classes, `FaceParsingClass`).
    /// The weights are MIT, but the CelebAMask-HQ data they were trained on is licensed
    /// for non-commercial research only: downloaded on first use, never bundled (Acknowledgements).
    /// The weights are inline in `model.mlmodel` (no `weight.bin`). Pinned by commit.
    /// The model card says `.all`, but on macOS 26 (M-series) predictions with `.all` or
    /// `.cpuAndNeuralEngine` fail ("Unable to compute the prediction", error -1);
    /// `.cpuAndGPU` gives the same classes as `.cpuOnly`.
    public static let faceParsing: DownloadableModelDescriptor = {
        let base = "https://huggingface.co/mlboydaisuke/Face-Parsing-CoreML/resolve/\(faceParsingRevision)/FaceParsing.mlpackage/"
        return DownloadableModelDescriptor(
            name: "FaceParsing", displayName: "face parsing model",
            files: [
                file(base, "Manifest.json", "4faf3700dad0497c6efd622ca031cb04d17ad8b685477403c8cd573effdb4a1a", 385),
                file(base, "Data/com.apple.CoreML/model.mlmodel",
                     "e7ebd6cc3f53486becc0dbf3b74027bc045aa4158402936ea09c3625682be6bb", 53_182_369),
            ],
            computeUnits: .cpuAndGPU, environmentKey: "DS_FACE_PARSING_MODEL_DIR", revision: faceParsingRevision)
    }()

    public static let faceParsingRevision = "6af21c2f8341fcebf9bf81f37bf13dd12aae302c"

    /// Depth Anything V2 Small (Lihe Yang et al.), Apple's Core ML conversion (float16),
    /// Apache-2.0 (Acknowledgements): input `image` (colour 518×392, stretched), output
    /// `depth` (one-component half float 518×392, relative inverse depth: higher = nearer).
    /// For the lens blur's depth map. Pinned by commit.
    public static let depthAnything: DownloadableModelDescriptor = {
        let base = "https://huggingface.co/apple/coreml-depth-anything-v2-small/resolve/\(depthAnythingRevision)/DepthAnythingV2SmallF16.mlpackage/"
        return DownloadableModelDescriptor(
            name: "DepthAnythingV2SmallF16", displayName: "depth model",
            files: [
                file(base, "Manifest.json", "2883ae290c48fe916dc5ececac03a7d847fa277165a49ef5652fa1d2b9cb55f7", 617),
                file(base, "Data/com.apple.CoreML/model.mlmodel",
                     "44ac97a3efcfd52113183fb2862ff59cd0368e9ec2e30a90a54980dd11407042", 399_433),
                file(base, "Data/com.apple.CoreML/weights/weight.bin",
                     "fa60d9b6a155734f59029ebb882fd54e549bfaee3539c1a9cbd2cbbab64a0fed", 49_419_072),
            ],
            computeUnits: .all, environmentKey: "DS_DEPTH_MODEL_DIR", revision: depthAnythingRevision)
    }()

    public static let depthAnythingRevision = "cfef6f6f2a70783dedc0bfae40cecbc2052285d3"
}

public enum ModelDownloadError: Error, Equatable, LocalizedError {
    case download(String, String)
    case checksum(String, String)
    case compile(String, String)

    public var errorDescription: String? {
        switch self {
        case .download(let n, let m): "Couldn't download the \(n): \(m)"
        case .checksum(let n, let f): "The downloaded \(n) is damaged (\(f)). Try again."
        case .compile(let n, let m): "Couldn't prepare the \(n): \(m)"
        }
    }
}

/// The one download lifecycle every `DownloadableModelDescriptor` shares (LaMa, face parsing, depth, sky):
/// download into the support folder on first use (progress, cancel, SHA-256 per file),
/// compile on this Mac with `MLModel.compileModel(at:)`, and load (a compiled copy that
/// doesn't load is tried once more, then discarded: the user's Try Again fetches it anew).
@MainActor @Observable
public final class DownloadableModel {
    public enum State: Equatable, Sendable {
        case notDownloaded
        case downloading(Double)
        case ready
        case failed(String)
    }

    public private(set) var state: State
    public let descriptor: DownloadableModelDescriptor
    @ObservationIgnored let root: URL
    @ObservationIgnored private let downloader: any ModelDownloader
    @ObservationIgnored private let compile: @Sendable (URL) async throws -> URL
    @ObservationIgnored private let load: @Sendable (URL, MLModelConfiguration) throws -> MLModel
    @ObservationIgnored private var preparing: Task<URL, Error>?
    @ObservationIgnored private var model: MLModel?
    @ObservationIgnored private var sweptStaging = false

    public init(root: URL, descriptor: DownloadableModelDescriptor,
                downloader: any ModelDownloader = URLSessionModelDownloader(),
                compile: (@Sendable (URL) async throws -> URL)? = nil,
                load: (@Sendable (URL, MLModelConfiguration) throws -> MLModel)? = nil) {
        self.root = root
        self.descriptor = descriptor
        self.downloader = downloader
        // Not default-argument function values: a default `compile` closure called from
        // this main-actor class crashed the task allocator ("freed pointer was not the
        // last allocation", Swift 6.2); closures formed here are fine.
        self.compile = compile ?? { try await MLModel.compileModel(at: $0) }
        self.load = load ?? { try MLModel(contentsOf: $0, configuration: $1) }
        Self.adoptLegacyCopy(in: root, descriptor: descriptor)
        state = FileManager.default.fileExists(atPath: root.appendingPathComponent(descriptor.compiledName).path)
            ? .ready : .notDownloaded
    }

    /// An earlier build's compiled copy (same pinned files) is renamed into place; if both exist
    /// the old one is removed. A rename is cheap, so it is done here rather than off the main actor.
    private static func adoptLegacyCopy(in root: URL, descriptor: DownloadableModelDescriptor) {
        guard let legacy = descriptor.legacyCompiledName, legacy != descriptor.compiledName else { return }
        let fm = FileManager.default
        let old = root.appendingPathComponent(legacy, isDirectory: true)
        let new = root.appendingPathComponent(descriptor.compiledName, isDirectory: true)
        guard fm.fileExists(atPath: old.path) else { return }
        if fm.fileExists(atPath: new.path) {
            try? fm.removeItem(at: old)
        } else {
            try? fm.moveItem(at: old, to: new)
        }
    }

    /// The app's copy in `<support>/Models`; the descriptor's environment variable (a
    /// local `.mlpackage`) replaces the network.
    public static func standard(_ descriptor: DownloadableModelDescriptor,
                                environment: [String: String] = ProcessInfo.processInfo.environment) -> DownloadableModel {
        let downloader: any ModelDownloader = environment[descriptor.environmentKey].map {
            LocalModelDownloader(package: URL(fileURLWithPath: $0))
        } ?? URLSessionModelDownloader()
        return DownloadableModel(root: AppPaths.support.appendingPathComponent("Models", isDirectory: true),
                                 descriptor: descriptor, downloader: downloader)
    }

    public var compiledURL: URL { root.appendingPathComponent(descriptor.compiledName, isDirectory: true) }

    public var isDownloading: Bool {
        if case .downloading = state { return true }
        return false
    }

    /// The compiled model on disk, downloading and compiling it first (one at a time).
    /// After `cancel()` this throws `CancellationError` and the state is `.notDownloaded`.
    public func prepare() async throws -> URL {
        if FileManager.default.fileExists(atPath: compiledURL.path) {
            state = .ready
            return compiledURL
        }
        if let preparing { return try await preparing.value }
        let task = Task { try await self.downloadAndCompile() }
        preparing = task
        defer { preparing = nil }
        do {
            let url = try await task.value
            state = .ready
            return url
        } catch {
            if error is CancellationError || task.isCancelled {
                state = .notDownloaded
                throw CancellationError()
            }
            state = .failed(Self.message(error))
            throw error
        }
    }

    /// Stop the download in progress; nothing is kept.
    public func cancel() {
        preparing?.cancel()
    }

    /// The loaded model (downloading it if needed). A compiled copy that doesn't load is
    /// tried once more (a load can fail transiently, e.g. under memory pressure); if it
    /// fails again it is deleted and the error reported, so the model is never "ready"
    /// while unusable. Downloading it again is the user's call (Try Again), not silent.
    public func loadedModel() async throws -> MLModel {
        if let model { return model }
        let url = try await prepare()
        if let m = try? await loadNow(url) { return m }
        do {
            return try await loadNow(url)
        } catch {
            discardCompiled()
            state = .failed(Self.message(error) + " Try again to download it anew.")
            throw error
        }
    }

    private func discardCompiled() {
        model = nil
        try? FileManager.default.removeItem(at: compiledURL)
        state = .notDownloaded
    }

    /// Loads off the main actor: the first load of LaMa specializes it for the Neural
    /// Engine (about 7 s on an M-series Mac).
    private func loadNow(_ url: URL) async throws -> MLModel {
        if let model { return model }
        let units = descriptor.computeUnits, load = self.load
        do {
            let box = try await Task.detached(priority: .userInitiated) {
                let config = MLModelConfiguration()
                config.computeUnits = units
                return LoadedModel(model: try load(url, config))
            }.value
            if let model { return model }
            model = box.model
            return box.model
        } catch {
            throw ModelDownloadError.compile(descriptor.displayName, error.localizedDescription)
        }
    }

    static func message(_ error: Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }

    private func downloadAndCompile() async throws -> URL {
        if !sweptStaging {
            // Before this instance's first staging folder exists, so only an earlier run's leftovers go.
            sweptStaging = true
            let root = root, prefix = descriptor.stagingPrefix
            await Task.detached(priority: .utility) {
                ModelFiles.sweepStaging(in: root, prefix: prefix)
                ModelFiles.sweepLegacyStaging(in: root)
            }.value
        }
        let staging = root.appendingPathComponent(descriptor.stagingPrefix + UUID().uuidString, isDirectory: true)
        let package = staging.appendingPathComponent(descriptor.name + ".mlpackage", isDirectory: true)
        let result: Result<URL, Error>
        do {
            result = .success(try await downloadAll(into: package))
        } catch {
            result = .failure(error)
        }
        try? FileManager.default.removeItem(at: staging)
        return try result.get()
    }

    private func downloadAll(into package: URL) async throws -> URL {
        let fm = FileManager.default
        try fm.createDirectory(at: package, withIntermediateDirectories: true)
        let total = max(descriptor.totalBytes, 1)
        var done: Int64 = 0
        state = .downloading(0)
        for file in descriptor.files {
            try Task.checkCancellation()
            try await download(file, into: package, total: total, before: done)
            done += file.size
            state = .downloading(Double(done) / Double(total))
        }
        try Task.checkCancellation()
        let compiled = try await Self.compile(package, with: compile, name: descriptor.displayName)
        // Cancelled while compiling: nothing is installed (the staging folder goes).
        do {
            try Task.checkCancellation()
        } catch {
            try? fm.removeItem(at: compiled)
            throw error
        }
        try? fm.removeItem(at: compiledURL)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        try fm.moveItem(at: compiled, to: compiledURL)
        return compiledURL
    }

    private nonisolated static func compile(_ package: URL, with compile: @Sendable (URL) async throws -> URL,
                                            name: String) async throws -> URL {
        do {
            return try await compile(package)
        } catch {
            throw ModelDownloadError.compile(name, error.localizedDescription)
        }
    }

    private func download(_ file: DownloadableModelDescriptor.File, into package: URL, total: Int64, before: Int64) async throws {
        let dst = package.appendingPathComponent(file.path)
        try FileManager.default.createDirectory(at: dst.deletingLastPathComponent(), withIntermediateDirectories: true)
        let report: @Sendable (Int64) -> Void = { [weak self] bytes in
            let f = min(1, Double(before + bytes) / Double(total))
            Task { @MainActor in if case .downloading = self?.state { self?.state = .downloading(f) } }
        }
        do {
            try await ModelFiles.fetch(file.url, to: dst, sha256: file.sha256, size: file.size, path: file.path,
                                       using: downloader, progress: report)
        } catch let e as ModelFetchError {
            switch e {
            case .download(let m): throw ModelDownloadError.download(descriptor.displayName, m)
            case .checksum(let f): throw ModelDownloadError.checksum(descriptor.displayName, f)
            }
        }
    }
}

/// An `MLModel` handed from the loading task to the main actor (predictions are thread-safe).
private struct LoadedModel: @unchecked Sendable {
    let model: MLModel
}
