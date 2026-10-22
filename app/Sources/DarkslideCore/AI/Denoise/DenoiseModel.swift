import CoreML
import Foundation
import Observation

/// The AI Denoise network: NAFNet-SIDD-width32 (megvii-research, MIT) in Core ML float16,
/// 512×512 planar RGB tiles in and out (`input` → `output`, sRGB-encoded 0..1). It is
/// converted when the app is built (`scripts/denoise-model/`, docs/DENOISE_MODEL.md) and
/// bundled in `Contents/Resources/Models`; nothing is downloaded. See `Acknowledgements.md`.
public enum DenoiseModelDescriptor {
    public static let name = "DarkslideDenoise"
    /// SHA-256 of the package's weights (`Data/com.apple.CoreML/weights/weight.bin`); the
    /// pinned conversion reproduces it bit for bit (`convert.py` EXPECTED_WEIGHTS_SHA256).
    /// Also the model's identity for cached results.
    public static let weightsSHA256 = "90a2538b1881275d5aa964b6aa6451baffe32b3167db3a526d9a123a3e818d5f"
    /// Weights inside a package.
    static let weightsPath = "Data/com.apple.CoreML/weights/weight.bin"

    /// The bundled package in `bundle`, if the build included it.
    public static func bundledPackage(in bundle: Bundle) -> URL? {
        let url = bundle.resourceURL?.appendingPathComponent("Models/\(name).mlpackage", isDirectory: true)
        return url.flatMap { FileManager.default.fileExists(atPath: $0.path) ? $0 : nil }
    }
}

public enum DenoiseError: Error, Equatable, LocalizedError {
    case missing
    case damaged
    case compile(String)
    case model(String)
    case denoise(String)

    public var errorDescription: String? {
        switch self {
        case .missing: "AI Denoise model not included in this build."
        case .damaged: "The AI Denoise model in this build is damaged (its checksum doesn't match)."
        case .compile(let m): "Couldn't prepare the AI Denoise model: \(m)"
        case .model(let m): "The AI Denoise model can't run: \(m)"
        case .denoise(let m): "AI Denoise failed: \(m)"
        }
    }
}

/// The bundled AI Denoise model: checked against its pinned weights checksum, then compiled
/// on this Mac once (kept in the support folder, keyed by the checksum). AI Upscale's model
/// (bundled, or downloaded on first use) uses this store too (its own name and reasons).
@MainActor @Observable
public final class DenoiseModelStore {
    public enum Availability: Equatable, Sendable {
        /// Bundled (and, once checked, matching its checksum).
        case available
        /// The build didn't include it.
        case missing
        /// Its weights don't match the pinned checksum: refused.
        case damaged
    }

    public private(set) var availability: Availability
    /// The model's identity for cached results (Rust `set_denoise_model`).
    public let id: String
    @ObservationIgnored private var package: URL?
    @ObservationIgnored private let compiledRoot: URL
    @ObservationIgnored private let name: String
    /// What the UI says when the model is missing or damaged (`nil`: AI Denoise's).
    @ObservationIgnored private let reasons: (missing: String, damaged: String)?
    @ObservationIgnored private let compile: @Sendable (URL) async throws -> URL
    @ObservationIgnored private var verified = false
    @ObservationIgnored private var preparing: Task<URL, Error>?

    /// `package`: the bundled `.mlpackage` (nil: not in this build); `expectedWeights`: its
    /// pinned weights checksum; compiled models go under `compiledRoot`.
    public init(package: URL?, compiledRoot: URL, expectedWeights: String = DenoiseModelDescriptor.weightsSHA256,
                name: String = DenoiseModelDescriptor.name, reasons: (missing: String, damaged: String)? = nil,
                compile: @escaping @Sendable (URL) async throws -> URL = ModelFiles.compile) {
        self.package = package
        self.compiledRoot = compiledRoot
        self.name = name
        self.reasons = reasons
        self.compile = compile
        id = expectedWeights
        availability = package == nil ? .missing : .available
    }

    /// Why AI Denoise can't be used, or nil.
    public var unavailableReason: String? {
        switch availability {
        case .available: nil
        case .missing: reasons?.missing ?? DenoiseError.missing.errorDescription
        case .damaged: reasons?.damaged ?? DenoiseError.damaged.errorDescription
        }
    }

    /// Where the compiled model goes.
    public var compiledURL: URL {
        compiledRoot.appendingPathComponent("\(name)-\(id.prefix(16)).mlmodelc", isDirectory: true)
    }

    /// Check the bundled package's weights against the pinned checksum (once). A mismatch
    /// marks the model damaged and throws.
    public func verify() async throws {
        guard let package else { throw DenoiseError.missing }
        if verified { return }
        let weights = package.appendingPathComponent(DenoiseModelDescriptor.weightsPath)
        let sha = try? await Task.detached(priority: .utility) { try ModelFiles.sha256(of: weights) }.value
        guard sha == id else {
            availability = .damaged
            throw DenoiseError.damaged
        }
        verified = true
    }

    /// The compiled model: verified, then compiled on first use (one at a time).
    public func prepare() async throws -> URL {
        try await verify()
        if FileManager.default.fileExists(atPath: compiledURL.path) { return compiledURL }
        if let preparing { return try await preparing.value }
        let task = Task { try await self.compileIntoPlace() }
        preparing = task
        defer { preparing = nil }
        return try await task.value
    }

    /// A package installed after launch (AI Upscale's download): used from now on, checked
    /// against the pinned checksum on first use like a bundled one.
    public func install(package: URL) {
        self.package = package
        verified = false
        availability = .available
    }

    /// The compiled model doesn't load (half-written, damaged): compile it again next time.
    public func discard() {
        try? FileManager.default.removeItem(at: compiledURL)
    }

    private func compileIntoPlace() async throws -> URL {
        guard let package else { throw DenoiseError.missing }
        let compiled: URL
        do {
            compiled = try await compile(package)
        } catch {
            throw DenoiseError.compile(error.localizedDescription)
        }
        let fm = FileManager.default
        try fm.createDirectory(at: compiledRoot, withIntermediateDirectories: true)
        try? fm.removeItem(at: compiledURL)
        try fm.moveItem(at: compiled, to: compiledURL)
        return compiledURL
    }
}
