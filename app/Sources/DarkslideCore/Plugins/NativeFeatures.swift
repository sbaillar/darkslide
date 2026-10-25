import Foundation
import Observation

/// The app's own optional integrations. Each is a bundled plug-in (`darkslide.<id>`,
/// `plugins/builtin/`) that Settings ▸ Plugins turns on or off; the code stays native
/// (docs/PLUGINS.md, "Native features"). Off, a feature's settings tab, menus, badges,
/// panels and background work go away; its settings and credentials are kept.
public enum NativeFeature: String, CaseIterable, Sendable, Identifiable {
    case archive
    case amazonPhotos = "amazon-photos"
    case immich
    case googlePhotos = "google-photos"

    public var id: String { rawValue }

    /// The bundled plug-in that switches it.
    public var pluginID: String { "darkslide.\(rawValue)" }

    public var title: String {
        switch self {
        case .archive: "Archive"
        case .amazonPhotos: "Amazon Photos"
        case .immich: "Immich"
        case .googlePhotos: "Google Photos"
        }
    }

    /// The feature a bundled plug-in switches, if it is one of these.
    public init?(pluginID: String) {
        guard pluginID.hasPrefix("darkslide."), let f = Self(rawValue: String(pluginID.dropFirst("darkslide.".count)))
        else { return nil }
        self = f
    }

    /// The one-time migration: whether the user had already set this feature up before
    /// features became plug-ins (then it starts on, so nothing they rely on stops).
    /// Archive: a target. Amazon Photos: a folder. Immich: a server. Google Photos:
    /// signed in, or their own OAuth client entered (the client built into the app
    /// doesn't count).
    @MainActor
    public func wasSetUp(settings: AppSettings, immich: ImmichSettings, google: GooglePhotosSettings) -> Bool {
        switch self {
        case .archive: !settings.archiveTarget.trimmingCharacters(in: .whitespaces).isEmpty
        case .amazonPhotos: settings.amazonPhotosFolder != nil
        case .immich: !immich.serverURL.isEmpty
        case .googlePhotos: !google.clientIDOverride.isEmpty || google.refreshToken != nil
        }
    }
}

/// Which integrations are on, for the views and models that hide or quiet them. In
/// the app it follows Settings ▸ Plugins (`follow(_:)`); unbound (tests, the demo)
/// every feature is on unless `set` says otherwise.
@MainActor @Observable
public final class FeatureGates {
    public private(set) var enabled: Set<NativeFeature>
    /// Settings ▸ Plugins, when bound: switching goes through the registry.
    @ObservationIgnored public private(set) weak var store: PluginStore?
    /// Called for each feature that turned on or off (the app starts or stops its work).
    @ObservationIgnored public var onChange: ((NativeFeature, Bool) -> Void)?

    public init(enabled: Set<NativeFeature> = Set(NativeFeature.allCases)) {
        self.enabled = enabled
    }

    public func isOn(_ feature: NativeFeature) -> Bool { enabled.contains(feature) }

    /// Replace the set; `onChange` hears each difference.
    public func update(_ new: Set<NativeFeature>) {
        guard new != enabled else { return }
        let old = enabled
        enabled = new
        for f in NativeFeature.allCases where old.contains(f) != new.contains(f) {
            onChange?(f, new.contains(f))
        }
    }

    /// Follow Settings ▸ Plugins from now on (the live app).
    public func follow(_ store: PluginStore) {
        self.store = store
        store.onFeaturesChanged = { [weak self, weak store] in
            guard let self, let store else { return }
            update(store.features)
        }
        update(store.features)
    }

    /// Turn a feature on or off (onboarding, a hint's button): through Settings ▸ Plugins
    /// when bound, so it is remembered; directly otherwise.
    public func set(_ feature: NativeFeature, _ on: Bool) async {
        if let store {
            await store.setEnabled(feature.pluginID, on)
        } else {
            update(on ? enabled.union([feature]) : enabled.subtracting([feature]))
        }
    }

    /// Whether a tool panel belongs to a feature that is off.
    public func shows(_ panel: ToolPanelID) -> Bool {
        switch panel {
        case .archive: isOn(.archive)
        case .amazonPhotos: isOn(.amazonPhotos)
        default: true
        }
    }
}
