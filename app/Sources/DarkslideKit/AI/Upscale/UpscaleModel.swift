import Foundation

/// The AI Upscale network: RealESRGAN_x2plus (Xintao Wang et al., Real-ESRGAN, BSD-3-Clause;
/// its RRDBNet from BasicSR, Apache-2.0) in Core ML float16, 256×256 planar RGB tiles in
/// and 512×512 out (`input` → `output`, sRGB-encoded 0..1). Converted by
/// `scripts/upscale-model/` (docs/UPSCALE_MODEL.md). Builds don't bundle it unless
/// `DS_BUNDLE_UPSCALE=1`: the app downloads the converted package on first use
/// (`UpscaleModelDownload`). See `Acknowledgements.md`.
public enum UpscaleModelDescriptor {
    public static let name = "DarkslideUpscale"
    /// SHA-256 of the package's weights; the pinned conversion reproduces it bit for bit
    /// (`convert.py` EXPECTED_WEIGHTS_SHA256).
    public static let weightsSHA256 = "d43bffce05fa1714f1a50e6ef19780f0ef5453a7e9448875316a4d564f46a310"
    /// What the UI says when this build can't upscale.
    public static let reasons = (missing: "AI Upscale model not included in this build.",
                                 damaged: "The AI Upscale model in this build is damaged (its checksum doesn’t match).")
    /// Why the menu offers the download again (a downloaded copy failed its check).
    public static let downloadDamaged = "The downloaded AI Upscale model is damaged. Photo ▸ Enhance ▸ Download AI Upscale Model… gets it again."
    /// Why the menu offers the download instead of Upscale 2×.
    public static let notDownloaded = "AI Upscale needs its model (about 34 MB). Photo ▸ Enhance ▸ Download AI Upscale Model… gets it."

    /// Where the converted package's files are published (`<base>Manifest.json`, …).
    /// Nil until the converted package is published (docs/UPSCALE_MODEL.md, "Download"): a
    /// build without `DS_UPSCALE_MODEL_URL` can't download the model.
    public static let downloadBase: String? = nil
    /// Overrides `downloadBase` (a URL ending in `/`).
    public static let downloadEnvironmentKey = "DS_UPSCALE_MODEL_URL"

    /// The package's files at `base`. The model spec and weights are pinned by SHA-256 (the
    /// weights are `weightsSHA256`, which the pinned conversion reproduces bit for bit). The
    /// manifest isn't pinned (an empty hash): each conversion writes new item UUIDs into it,
    /// so it is checked for its shape instead (`isPackageManifest`).
    public static func files(base: URL) -> [DownloadableModelDescriptor.File] {
        [("Manifest.json", "", 617),
         ("Data/com.apple.CoreML/model.mlmodel", "d038b721c093ce67854d70132787c89dad703d8079584a3fb0004454b3489cb0", 646_966),
         (DenoiseModelDescriptor.weightsPath, weightsSHA256, 33_451_648)].map {
            .init(path: $0.0, url: base.appendingPathComponent($0.0), sha256: $0.1, size: Int64($0.2))
        }
    }

    /// The item paths (relative to `Data/`) a package manifest may name: the pinned spec and weights.
    static let manifestItemPaths: Set<String> = ["com.apple.CoreML/model.mlmodel", "com.apple.CoreML/weights"]

    /// A Core ML package manifest: JSON with `itemInfoEntries` and a `rootModelIdentifier`
    /// naming one of them, every entry's `path` one of `manifestItemPaths` (so the unpinned
    /// manifest can't point Core ML at anything but the pinned files).
    nonisolated static func isPackageManifest(_ url: URL) -> Bool {
        guard let data = try? Data(contentsOf: url),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let entries = json["itemInfoEntries"] as? [String: Any], !entries.isEmpty,
              let root = json["rootModelIdentifier"] as? String, entries[root] != nil else { return false }
        return entries.values.allSatisfy { entry in
            guard let path = (entry as? [String: Any])?["path"] as? String else { return false }
            return manifestItemPaths.contains(path) // exact match: no "..", no absolute path
        }
    }

    /// The download location: the environment's, else `downloadBase`; nil when neither is set.
    public static func downloadURL(environment: [String: String] = ProcessInfo.processInfo.environment) -> URL? {
        (environment[downloadEnvironmentKey] ?? downloadBase).flatMap { $0.isEmpty ? nil : URL(string: $0) }
    }

    /// What the download prompt says (once, before anything is fetched).
    public static let downloadNotice = """
        Darkslide will download RealESRGAN_x2plus (BSD 3-Clause License, about 34 MB) and keep it on this Mac. \
        Its training data (DIV2K, Flickr2K, OST) is licensed for non-commercial research only. Darkslide's \
        Apache-2.0 licence doesn't override those terms; complying with them is up to you. Details are in \
        Darkslide ▸ About Darkslide.
        """

    /// The bundled package in `bundle`, if the build included it.
    public static func bundledPackage(in bundle: Bundle) -> URL? {
        let url = bundle.resourceURL?.appendingPathComponent("Models/\(name).mlpackage", isDirectory: true)
        return url.flatMap { FileManager.default.fileExists(atPath: $0.path) ? $0 : nil }
    }
}

/// AI Upscale's model in builds that don't bundle it: its pinned files downloaded on the
/// user's request into a staging folder, each checked by SHA-256, then moved into place
/// as `<root>/DarkslideUpscale.mlpackage` in one rename (a cancel or failure leaves
/// nothing). `DenoiseModelStore` then checks and compiles it like a bundled one.
@MainActor @Observable
public final class UpscaleModelDownload {
    public enum State: Equatable, Sendable {
        case notDownloaded
        case downloading(Double)
        case installed
        case failed(String)
    }

    public private(set) var state: State
    /// The files to fetch; nil when this build has no download location.
    @ObservationIgnored let files: [DownloadableModelDescriptor.File]?
    @ObservationIgnored let root: URL
    @ObservationIgnored private let downloader: any ModelDownloader
    @ObservationIgnored private var running: Task<URL, Error>?
    @ObservationIgnored private var sweptStaging = false

    public init(root: URL, files: [DownloadableModelDescriptor.File]?,
                downloader: any ModelDownloader = URLSessionModelDownloader()) {
        self.root = root
        self.files = files
        self.downloader = downloader
        state = .notDownloaded
        if FileManager.default.fileExists(atPath: packageURL.path) { state = .installed }
    }

    /// Staging folders are `<root>/.upscale-download-<uuid>`: other models' sweeps of
    /// `.download-*` never take one in flight.
    static let stagingPrefix = ".upscale-download-"

    /// The app's: into `<support>/Models`, from `UpscaleModelDescriptor.downloadURL`.
    public static func standard(environment: [String: String] = ProcessInfo.processInfo.environment) -> UpscaleModelDownload {
        UpscaleModelDownload(root: AppPaths.support.appendingPathComponent("Models", isDirectory: true),
                             files: UpscaleModelDescriptor.downloadURL(environment: environment)
                                 .map(UpscaleModelDescriptor.files(base:)))
    }

    /// Where the package is installed.
    public var packageURL: URL {
        root.appendingPathComponent(UpscaleModelDescriptor.name + ".mlpackage", isDirectory: true)
    }

    /// The installed package, if any.
    public var installedPackage: URL? { state == .installed ? packageURL : nil }

    public var isConfigured: Bool { files != nil }

    public var isDownloading: Bool {
        if case .downloading = state { return true }
        return false
    }

    /// Fetch, check and install the package (one run at a time); returns it. After
    /// `cancel()` this throws `CancellationError` and nothing is installed.
    public func download() async throws -> URL {
        if let running { return try await running.value }
        guard let files else { throw UpscaleError.unavailable(UpscaleModelDescriptor.reasons.missing) }
        state = .downloading(0)
        let task = Task { try await self.fetch(files) }
        running = task
        defer { running = nil }
        do {
            let url = try await task.value
            state = .installed
            return url
        } catch {
            if error is CancellationError || task.isCancelled {
                state = .notDownloaded
                throw CancellationError()
            }
            state = .failed(DownloadableModel.message(error))
            throw error
        }
    }

    public func cancel() {
        running?.cancel()
    }

    private func fetch(_ files: [DownloadableModelDescriptor.File]) async throws -> URL {
        let fm = FileManager.default
        if !sweptStaging { // a download a crash or quit cut short, before this one's folder exists
            sweptStaging = true
            let root = root, prefix = Self.stagingPrefix
            await Task.detached(priority: .utility) { ModelFiles.sweepStaging(in: root, prefix: prefix) }.value
        }
        let staging = root.appendingPathComponent(Self.stagingPrefix + UUID().uuidString, isDirectory: true)
        defer { try? fm.removeItem(at: staging) }
        let package = staging.appendingPathComponent(packageURL.lastPathComponent, isDirectory: true)
        let total = max(files.reduce(0) { $0 + $1.size }, 1)
        var done: Int64 = 0
        for file in files {
            try Task.checkCancellation()
            let dst = package.appendingPathComponent(file.path)
            try fm.createDirectory(at: dst.deletingLastPathComponent(), withIntermediateDirectories: true)
            let before = done
            let name = "AI Upscale model"
            do {
                try await ModelFiles.fetch(file.url, to: dst, sha256: file.sha256, size: file.size, path: file.path,
                                           using: downloader, progress: { [weak self] bytes in
                    let f = min(1, Double(before + bytes) / Double(total))
                    Task { @MainActor in if case .downloading = self?.state { self?.state = .downloading(f) } }
                }, check: file.sha256.isEmpty ? { @Sendable url in UpscaleModelDescriptor.isPackageManifest(url) } : nil)
            } catch let e as ModelFetchError {
                switch e {
                case .download(let m): throw ModelDownloadError.download(name, m)
                case .checksum(let f): throw ModelDownloadError.checksum(name, f)
                }
            }
            done += file.size
            state = .downloading(Double(done) / Double(total))
        }
        try Task.checkCancellation()
        if fm.fileExists(atPath: packageURL.path) {
            _ = try fm.replaceItemAt(packageURL, withItemAt: package) // a damaged earlier copy, swapped atomically
        } else {
            try fm.moveItem(at: package, to: packageURL)
        }
        return packageURL
    }
}

public enum UpscaleError: Error, Equatable, LocalizedError {
    /// The model is missing or damaged (the reason), or the session can't take new photos.
    case unavailable(String)
    case failed(String)

    public var errorDescription: String? {
        switch self {
        case .unavailable(let m): m
        case .failed(let m): "AI Upscale failed: \(m)"
        }
    }
}
