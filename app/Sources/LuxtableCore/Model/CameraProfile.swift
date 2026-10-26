import LuxtableFFI
import Foundation

/// The base look of an edit (lt-core `Adjustments.profile`): a Luxtable profile or an
/// imported Adobe camera profile (`.dcp`), referenced by the file's BLAKE3 hash.
public enum CameraProfileRef: Equatable, Hashable, Sendable {
    case standard, neutral, vivid
    case dcp(hash: String, name: String)
    /// A profile this build doesn't know (written by a newer one, or an unreadable
    /// import), kept verbatim as canonical JSON so a save never downgrades it (lt-core
    /// `Profile::Other`). It renders as Standard.
    case other(json: String)

    /// What an edit without a `profile` key means (lt-core `Profile::legacy`): sidecars
    /// from before profiles keep the look they were edited under.
    public static let legacy: CameraProfileRef = .neutral

    /// Read the sidecar's `profile` object: missing (or not an object) → [`legacy`]; an
    /// object of a kind this build can't read → [`other`], unchanged.
    public init(json: Any?) {
        guard let o = json as? [String: Any] else {
            self = Self.legacy
            return
        }
        switch (o["kind"] as? String, o["hash"] as? String) {
        case ("standard", _): self = .standard
        case ("neutral", _): self = .neutral
        case ("vivid", _): self = .vivid
        case let ("dcp", hash?): self = .dcp(hash: hash, name: (o["name"] as? String) ?? "Camera Profile")
        default:
            let data = try? JSONSerialization.data(withJSONObject: o, options: [.sortedKeys])
            self = .other(json: data.flatMap { String(data: $0, encoding: .utf8) } ?? "{}")
        }
    }

    /// The sidecar's `profile` object.
    public var json: [String: Any] {
        switch self {
        case .standard: ["kind": "standard"]
        case .neutral: ["kind": "neutral"]
        case .vivid: ["kind": "vivid"]
        case let .dcp(hash, name): ["kind": "dcp", "hash": hash, "name": name]
        case let .other(json):
            (try? JSONSerialization.jsonObject(with: Data(json.utf8))) as? [String: Any] ?? [:]
        }
    }

    public var displayName: String {
        switch self {
        case .standard: "Luxtable Standard"
        case .neutral: "Luxtable Neutral"
        case .vivid: "Luxtable Vivid"
        case let .dcp(_, name): name
        case .other: "Unknown Profile"
        }
    }

    init?(entry: CameraProfileEntry) {
        switch entry.kind {
        case "standard": self = .standard
        case "neutral": self = .neutral
        case "vivid": self = .vivid
        case "dcp":
            guard let hash = entry.hash else { return nil }
            self = .dcp(hash: hash, name: entry.name)
        default: return nil
        }
    }
}

/// One choice in the profile picker.
public struct CameraProfileOption: Identifiable, Equatable, Sendable {
    public let profile: CameraProfileRef
    public let name: String
    /// The camera an imported profile was made for.
    public let camera: String?

    public var id: CameraProfileRef { profile }
    public var isImported: Bool { if case .dcp = profile { true } else { false } }
}

/// Imported camera profiles: copied into `<support>/Profiles/<hash>.dcp` and registered
/// with the Rust pipeline (at launch for the ones already there).
public enum CameraProfileLibrary {
    public static var folder: URL { AppPaths.support.appendingPathComponent("Profiles", isDirectory: true) }

    /// Register every profile in `folder` (blocking: reads and parses every file).
    /// Returns how many were loaded.
    @discardableResult
    public static func loadInstalled(from folder: URL = folder) -> Int {
        ((try? loadCameraProfiles(dir: folder.path)) ?? []).count
    }

    /// The launch registration, off the main thread (`startLoading`); a session opens
    /// once it is done, so no render of an imported profile falls back to Standard.
    @MainActor public private(set) static var loading: Task<Int, Never>?

    @MainActor public static func startLoading(from folder: URL = folder) {
        loading = Task.detached(priority: .userInitiated) { loadInstalled(from: folder) }
    }

    /// Luxtable's profiles, then the imported ones by name.
    public static func options() -> [CameraProfileOption] {
        ((try? cameraProfiles()) ?? []).compactMap { e in
            CameraProfileRef(entry: e).map { CameraProfileOption(profile: $0, name: e.name, camera: e.camera) }
        }
    }

    /// Copy the `.dcp` at `url` into `folder` and register it, off the main thread.
    public static func importProfile(at url: URL, into folder: URL = folder) async throws -> CameraProfileOption {
        let e = try await Task.detached(priority: .userInitiated) {
            try importCameraProfile(path: url.path, dir: folder.path)
        }.value
        guard let ref = CameraProfileRef(entry: e) else { throw CocoaError(.fileReadCorruptFile) }
        return CameraProfileOption(profile: ref, name: e.name, camera: e.camera)
    }

    /// False for an imported profile whose file isn't loaded, or one this build doesn't
    /// know (both render as Standard).
    public static func isAvailable(_ p: CameraProfileRef) -> Bool {
        switch p {
        case let .dcp(hash, _): cameraProfileAvailable(hash: hash)
        case .other: false
        default: true
        }
    }

    /// Whether `p` renders as itself on a photo from `camera` (`Photo.meta.camera`): an
    /// imported profile made for another camera renders as Standard (lt-pipeline M2).
    public static func fits(_ p: CameraProfileRef, camera: String) -> Bool {
        switch p {
        case let .dcp(hash, _): cameraProfileFits(hash: hash, camera: camera)
        case .other: false
        default: true
        }
    }

    /// The picker's choices for an edit currently on `current`: a missing imported
    /// profile stays listed (marked) so the menu shows what the edit uses.
    public static func options(current: CameraProfileRef) -> [CameraProfileOption] {
        var all = options()
        if !all.contains(where: { $0.profile == current }) {
            all.append(CameraProfileOption(profile: current, name: current.displayName + " (missing)", camera: nil))
        }
        return all
    }
}
