import Foundation

// MARK: - Optional integrations (Settings ▸ Plugins)

extension AppModel {
    /// The first launch with the integrations as plug-ins decides each once
    /// (`PluginStore.migrateFeatures`): on when the user had already set it up, so
    /// nothing they rely on stops after the update; off for a new user. The live app
    /// runs it before anything asks whether a feature is on.
    @discardableResult
    public static func migrateFeatures(_ store: PluginStore, settings: AppSettings,
                                       google: GooglePhotosSettings? = nil) -> [String] {
        let immich = ImmichSettings(defaults: settings.userDefaults)
        let google = google ?? GooglePhotosSettings(defaults: settings.userDefaults)
        return store.migrateFeatures { $0.wasSetUp(settings: settings, immich: immich, google: google) }
    }

    /// The live app: the integrations follow Settings ▸ Plugins from now on.
    public func followPlugins(_ store: PluginStore) {
        features.follow(store)
    }

    /// A feature was turned on or off: start or stop its work at once (no relaunch).
    /// Turning one off never deletes its settings, credentials or data.
    /// - Archive and Amazon Photos: the Rust driver asks the registry before each run
    ///   or hand-off (`ArchiveManager.set_plugin_host`); here only the UI follows.
    /// - Immich: the album watch, the sidecars and the developed copies stop or resume.
    /// - Google Photos: the Export sheet asks before each upload.
    func featureChanged(_ feature: NativeFeature, _ on: Bool) {
        switch feature {
        case .archive:
            if !on { archivePopoverShown = false }
        case .immich:
            immich.featureChanged(on)
        case .amazonPhotos, .googlePhotos:
            break
        }
    }
}
