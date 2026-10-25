import DarkslideFFI
import Foundation
import Synchronization
import Observation

/// The app's one plug-in host (also the one export steps resolve through): opened on
/// first use, from any thread.
public enum PluginHosts {
    public static let shared = PluginHost.open(builtinDir: AppPaths.builtInPlugins?.path, userDir: AppPaths.plugins.path)
}

/// What the store shows, read off the main thread after each change.
struct PluginSnapshot: Sendable {
    var plugins: [PluginRecord]
    var postProcessors: [PostProcessorInfo]
    var allowUnsignedCode: Bool
    var devFolders: [String]
    var content: PluginContent
    var features: Set<NativeFeature>

    init(_ host: PluginHost) {
        plugins = host.list()
        features = Set(host.nativeFeatures().compactMap(NativeFeature.init(rawValue:)))
        postProcessors = host.postProcessors()
        allowUnsignedCode = host.allowUnsignedCode()
        devFolders = host.devFolders()
        content = PluginContent.load(from: host)
    }
}

/// Settings ▸ Plugins: the plug-ins found, what they wait for, and the user's decisions
/// (ds-ffi `PluginHost`, which keeps them in `<support>/Plugins/state.json`). Every
/// change runs off the main actor (a reload re-reads and re-checks every package), then
/// the app's `PluginContent` is replaced and `onContentChanged` runs.
@MainActor @Observable
public final class PluginStore {
    public static let shared = PluginStore(host: PluginHosts.shared)

    public private(set) var plugins: [PluginRecord] = []
    public private(set) var postProcessors: [PostProcessorInfo] = []
    public private(set) var allowUnsignedCode = false
    public private(set) var devFolders: [String] = []
    /// The app's own integrations that are on (`FeatureGates` follows this).
    public private(set) var features: Set<NativeFeature> = []
    /// The last problem (a refused install, a bad pack), shown in the pane.
    public var lastError: String?
    /// Packs that didn't load.
    public private(set) var contentErrors: [String] = []
    /// Called after the active content changed (voice phrases to refresh, panels to redraw).
    @ObservationIgnored public var onContentChanged: (() -> Void)?
    /// Called after the plug-ins were read again (an integration may have been turned on or off).
    @ObservationIgnored public var onFeaturesChanged: (() -> Void)?
    /// Folders no file grant may reach besides Darkslide's own: the app's session roots.
    @ObservationIgnored public var protectedRoots: () -> [URL] = { [] }
    @ObservationIgnored let host: PluginHost
    @ObservationIgnored private let trash: @Sendable (URL) throws -> Void
    /// Actions and their snapshots are numbered in the order they ran on the host; an
    /// older snapshot arriving after a newer one is dropped (rapid toggles can't reorder).
    nonisolated private static let order = Mutex<UInt64>(0)
    @ObservationIgnored private var shown: UInt64 = 0

    public init(host: PluginHost,
                trash: @escaping @Sendable (URL) throws -> Void = { try FileManager.default.trashItem(at: $0, resultingItemURL: nil) }) {
        self.host = host
        self.trash = trash
        lastError = host.stateError()
        apply(PluginSnapshot(host))
    }

    /// A store over its own host (tests: a temporary Plugins folder).
    public convenience init(builtinDir: URL?, userDir: URL,
                            trash: @escaping @Sendable (URL) throws -> Void = { try FileManager.default.trashItem(at: $0, resultingItemURL: nil) }) {
        self.init(host: PluginHost.open(builtinDir: builtinDir?.path, userDir: userDir.path), trash: trash)
    }

    private func apply(_ s: PluginSnapshot) {
        plugins = s.plugins
        postProcessors = s.postProcessors
        allowUnsignedCode = s.allowUnsignedCode
        devFolders = s.devFolders
        features = s.features
        contentErrors = s.content.errors
        PluginContent.current = s.content
        onContentChanged?()
        onFeaturesChanged?()
    }

    /// The first launch with integrations as plug-ins: each feature nobody has decided
    /// about yet starts on when `wasSetUp` says the user had set it up, else off; decided
    /// once and remembered. Later launches find nothing undecided and ask nothing (no
    /// Keychain reads). Runs at launch, before the features are first used.
    @discardableResult
    public func migrateFeatures(_ wasSetUp: (NativeFeature) -> Bool) -> [String] {
        let undecided = host.undecidedFeatures().compactMap(NativeFeature.init(rawValue:))
        guard !undecided.isEmpty else { return [] }
        let configured = Dictionary(uniqueKeysWithValues: undecided.map { ($0.rawValue, wasSetUp($0)) })
        let host = self.host
        // In order with every other change (an older snapshot still in flight is dropped).
        let (decided, snapshot, number) = Self.order.withLock { n in
            let decided = Result { try host.seedFeatures(configured: configured) }
            n += 1
            return (decided, PluginSnapshot(host), n)
        }
        shown = max(shown, number)
        apply(snapshot)
        switch decided {
        case let .success(ids): return ids
        case let .failure(error):
            lastError = Self.message(error)
            return []
        }
    }

    /// Re-read the records off the main actor (no change).
    public func refresh() async { await perform { _ in } }

    /// Look at the folders again (a plug-in added or changed by hand, a development folder).
    public func reload() async { await perform { $0.reload() } }

    /// Run `action` on the host off the main actor, then show the result.
    @discardableResult
    private func perform(_ action: @escaping @Sendable (PluginHost) throws -> Void) async -> Bool {
        let host = self.host
        let (snapshot, error, number) = await Task.detached {
            Self.order.withLock { n in
                var failure: Error?
                do { try action(host) } catch { failure = error }
                n += 1
                return (PluginSnapshot(host), failure.map(Self.message), n)
            }
        }.value
        lastError = error
        if number > shown {
            shown = number
            apply(snapshot)
        }
        return error == nil
    }

    /// A plug-in error in words.
    nonisolated static func message(_ error: Error) -> String {
        guard let e = error as? DsError else { return error.localizedDescription }
        switch e {
        case let .InvalidArgument(message), let .Io(message), let .Unsupported(message), let .FolderMoved(message),
             let .Internal(message): return message
        case .Cancelled: return "Cancelled."
        }
    }

    /// Copy the package at `url` (a folder or a `.dsplugin` zip) into staging and say
    /// what installing it would do; nil (with `lastError`) when it isn't a valid plug-in.
    /// Install or `cancel` the preview afterwards.
    public func preview(_ url: URL) async -> PluginInstallPreview? {
        let host = self.host
        let r: Result<PluginInstallPreview, Error> = await Task.detached {
            Result { try host.preview(path: url.path) }
        }.value
        switch r {
        case let .success(p):
            lastError = nil
            return p
        case let .failure(e):
            lastError = Self.message(e)
            return nil
        }
    }

    /// Install a confirmed preview: exactly the staged copy the sheet showed (the user's
    /// own folder is never touched).
    @discardableResult
    public func install(_ preview: PluginInstallPreview) async -> PluginInstallResult? {
        let box = ResultBox()
        await perform { box.value = try $0.install(token: preview.token, allowDowngrade: preview.downgrade) }
        return box.value
    }

    /// The install sheet was cancelled.
    public func cancel(_ preview: PluginInstallPreview) async {
        await perform { $0.discard(token: preview.token) }
    }

    /// Load a plug-in folder in place, for its author (reloads without re-approval; DEV).
    public func addDevFolder(_ url: URL) async { await perform { try $0.addDevFolder(path: url.path) } }
    public func removeDevFolder(_ path: String) async { await perform { try $0.removeDevFolder(path: path) } }

    public func setEnabled(_ id: String, _ on: Bool) async { await perform { try $0.setEnabled(id: id, enabled: on) } }
    public func approveFiles(_ id: String) async { await perform { try $0.approveFiles(id: id) } }
    /// Grant one permission; `folder` is the folder chosen for a `files.*` one.
    public func grant(_ id: String, _ permission: String, folder: URL? = nil) async {
        let protected = protectedRoots().map(\.path)
        await perform { try $0.grant(id: id, permission: permission, folder: folder?.path, protected: protected) }
    }
    public func revoke(_ id: String, _ permission: String) async { await perform { try $0.revoke(id: id, permission: permission) } }
    public func trustSigner(key: String, name: String) async { await perform { try $0.trustSigner(key: key, name: name) } }
    public func untrustSigner(key: String) async { await perform { try $0.untrustSigner(key: key) } }
    /// Settings ▸ Plugins ▸ "Allow unsigned code plug-ins" (for authors: every unsigned plug-in).
    public func setAllowUnsignedCode(_ on: Bool) async { await perform { try $0.setAllowUnsignedCode(allow: on) } }

    /// Move an installed plug-in to the Trash (built-ins can only be disabled); its
    /// grants and pinned signer go with it.
    public func remove(_ id: String) async {
        let trash = self.trash
        await perform { host in
            let folder = try host.userFolder(id: id)
            try trash(URL(fileURLWithPath: folder, isDirectory: true))
            try host.forget(id: id)
        }
    }

    /// One approval: trust the signer's key, accept the files, grant what it asks for,
    /// or allow this one unsigned plug-in (never the global switch).
    public func approve(_ record: PluginRecord, _ approval: PluginApproval, folder: URL? = nil) async {
        let protected = protectedRoots().map(\.path)
        await perform { host in
            switch approval {
            case .changed, .notInstalled: try host.approveFiles(id: record.id)
            case let .trustSigner(signer, key, _): try host.trustSigner(key: key, name: signer)
            case .allowUnsignedCode: try host.allowUnsigned(id: record.id)
            case let .grant(permission):
                try host.grant(id: record.id, permission: permission, folder: folder?.path, protected: protected)
            }
        }
    }

    /// Notes for a recipe step whose values an update no longer allows ("Width: … 0.2 is used").
    public func notes(for step: RecipePostProcess) -> [String] {
        host.stepNotes(pluginId: step.pluginID, contributionId: step.contributionID, paramsJson: step.spec.paramsJson)
    }
}

/// Carries a value out of a `@Sendable` closure.
private final class ResultBox: @unchecked Sendable {
    var value: PluginInstallResult?
}

extension PluginApproval {
    /// The approval in words, for its button's row.
    public var title: String {
        switch self {
        case .changed: "Its files changed since it was installed."
        case .notInstalled: "It was added to the Plugins folder by hand."
        case let .trustSigner(signer, _, fingerprint):
            "Signed with key \(fingerprint) (it claims to be “\(signer)”), which you haven't trusted yet."
        case .allowUnsignedCode: "It runs code and isn't signed: allow this plug-in only if you know where it came from."
        case let .grant(permission): "Asks for \(permission)."
        }
    }

    /// Whether granting it needs a folder the user chooses.
    public var needsFolder: Bool {
        switch self {
        case let .grant(permission): permission.hasPrefix("files.")
        default: false
        }
    }

    public var action: String {
        switch self {
        case .changed, .notInstalled: "Approve"
        case .trustSigner: "Trust Signer"
        case .allowUnsignedCode: "Allow"
        case .grant: "Grant"
        }
    }
}

extension PluginRecord {
    /// "4 looks · 1 export step" for the row.
    public var contributionSummary: String {
        let counts = Dictionary(grouping: contributions, by: \.kind).mapValues(\.count)
        let names: [(String, String, String)] = [
            ("looks", "look pack", "look packs"), ("styles", "style pack", "style packs"),
            ("voice_phrases", "phrase pack", "phrase packs"), ("export_post_process", "export step", "export steps"),
            ("export_destination", "export destination", "export destinations"),
            ("native_feature", "integration", "integrations"),
        ]
        return names.compactMap { kind, one, many in
            counts[kind].map { "\($0) \($0 == 1 ? one : many)" }
        }.joined(separator: " · ")
    }

    /// One of the app's own integrations (Archive, Immich…): listed under Integrations.
    public var isIntegration: Bool { builtIn && contributions.contains { $0.kind == "native_feature" } }

    /// What an integration reaches beyond the app, in words.
    public var uses: [String] { contributions.flatMap(\.uses) }

    /// "Built-in", "Key 3F2A-… (“Jane”)", "Unsigned", and DEV for a development folder.
    public var trustLabel: String {
        let base = switch trust {
        case .builtIn: "Built-in"
        case let .signed(signer, _, fingerprint): "Key \(fingerprint) (“\(signer)”)"
        case let .untrusted(signer, _, fingerprint): "Key \(fingerprint) (“\(signer)”, not trusted)"
        case .unsigned: "Unsigned"
        }
        return dev ? "DEV · \(base)" : base
    }

    public var stateLabel: String {
        switch state {
        case .active: "On"
        case .disabled: "Off"
        case .needsApproval: "Waiting for you"
        case .shadowed: "Replaced by another copy"
        case .unsupported: "Not for this Mac"
        case .error: "Didn't load"
        }
    }
}

/// The install sheet's lines.
public enum PluginInstallText {
    public static func signer(_ p: PluginInstallPreview) -> String {
        switch p.trust {
        case .builtIn: "Part of Darkslide."
        case let .signed(signer, _, fingerprint): "Signed with key \(fingerprint), which you trust (it claims to be “\(signer)”)."
        case let .untrusted(signer, _, fingerprint):
            "Signed with key \(fingerprint) (it claims to be “\(signer)”). You'll be asked whether to trust that key."
        case .unsigned: p.hasCode ? "Not signed. It runs code: only install it if you know where it came from."
            : "Not signed. It holds only data (no code)."
        }
    }

    public static func adds(_ p: PluginInstallPreview) -> String {
        "Adds: " + (p.contributions.isEmpty ? "nothing this Darkslide supports"
            : p.contributions.map(\.name).joined(separator: ", "))
    }

    public static func replaces(_ p: PluginInstallPreview) -> String? {
        guard let old = p.replaces else { return nil }
        return p.downgrade ? "Version \(old) is installed: this is older and replaces it."
            : "Replaces version \(old)."
    }
}
