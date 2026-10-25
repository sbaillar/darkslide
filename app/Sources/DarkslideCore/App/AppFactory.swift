import Foundation

extension AppModel {
    /// `--demo` selects mock data; anything else is live.
    public static func mode(arguments: [String]) -> AppMode {
        arguments.contains("--demo") ? .demo : .live
    }

    /// Wiring for each mode.
    ///
    /// - live: real RAW folders through ds-raw (metadata + embedded previews); the viewer
    ///   shows the embedded preview at once, then the ds-pipeline develop render (GPU,
    ///   CPU fallback), Vision AI.
    ///   Import copies and verifies through ds-import into sessions in the sessions
    ///   folder; the archive runs through ds-archive (rsync + the Amazon Photos hand-off).
    ///   Ratings, tags, picks and edits persist in ds-session sidecars.
    /// - demo: generated sessions, simulated import and archive.
    public static func make(mode: AppMode, settings: AppSettings = AppSettings(),
                            defaults: UserDefaults = .standard) -> AppModel {
        switch mode {
        case .live:
            // Settings ▸ Plugins: the integrations (archive, Amazon Photos, Immich, Google
            // Photos) are decided before anything asks whether they are on.
            let plugins = PluginStore.shared
            migrateFeatures(plugins, settings: settings)
            let previews = RustPreviewProvider()
            // ds-session through the FFI; folders open in place (index + sidecars in <folder>/.ds/).
            // Newly registered files get the user's "apply on import" preset.
            let store = RustSessionStore(recents: RecentFolders(defaults: defaults),
                                         storeBase: RustSessionStore.defaultStoreBase,
                                         autoApply: { await MainActor.run { PresetStore.shared.autoApplyPreset } },
                                         // New Session… goes where imports put new sessions.
                                         libraryRoot: { await MainActor.run {
                                             RustImportService.libraryRoot(sessionsFolder: settings.sessionsFolder)
                                         } })
            let develop = DevelopRenderer() // ds-pipeline through the shared GPU renderer
            DevelopRenderer.warmUp() // device + kernels off the main thread at launch
            let model = AppModel(
                sessions: store,
                images: PreviewThumbnailProvider(previews: previews),
                renderer: develop,
                // ds-import: verified copies into sessions, registered + deduped via the global index.
                // New sessions go to the sessions folder chosen in onboarding (read at each import).
                importer: RustImportService(
                    libraryRoot: { await MainActor.run { RustImportService.libraryRoot(sessionsFolder: settings.sessionsFolder) } },
                    autoApply: { await MainActor.run { PresetStore.shared.autoApplyPreset } },
                    metadataPreset: { await MainActor.run { MetadataPresetStore.shared.importPreset?.fields } }),
                // ds-archive: rsync in the background + the Amazon Photos hand-off.
                archive: RustArchiveService(initial: settings.archiveSnapshot,
                                            settings: { await MainActor.run { settings.archiveSnapshot } },
                                            handles: { await store.archiveHandle(for: $0) },
                                            lockDir: RustArchiveService.defaultLockDir,
                                            followAmazonFolder: { _ = await settings.followMovedAmazonFolder() },
                                            plugins: PluginHosts.shared),
                ai: makeAIService(),
                settings: settings,
                previews: previews,
                mode: .live)
            model.followPlugins(plugins)
            var exporter = RustExportService() // ds-export, RAWs developed by ds-pipeline
            exporter.maskProvider = { [weak model] id, targets in
                guard let model else { return [:] }
                return await model.viewer.masksForRender(photo: id, targets: targets)
            }
            // Removals export as previewed: LaMa on disk is waited for, never a silent PatchMatch.
            exporter.removalCheck = { await RemovalModel.shared.exportProblem() }
            model.exporter = exporter
            // Google Photos export destination: the user's own OAuth client, token in the Keychain.
            model.googlePhotos = RustGooglePhotosService(settings: GooglePhotosSettings(defaults: settings.userDefaults))
            // Immich: developed copies and sidecars in the archive, rule albums synced after archive runs.
            model.immich.configure(service: RustImmichService(manager: (model.archive as? RustArchiveService)?.manager,
                                                              handles: { await store.archiveHandle(for: $0) }),
                                   app: model)
            // Auto WB/tone, WB picker, as-shot + lens readouts, edited thumbnails (pipeline).
            let tools = RustDevelopTools()
            model.tools = tools
            // AI mask bitmaps for the pipeline's MaskSource (Vision, cached per photo/target).
            develop.maskProvider = { [weak model] id, targets in
                guard let model else { return [:] }
                return await model.viewer.masksForRender(photo: id, targets: targets)
            }
            // Edited grid thumbnails render their AI-mask layers with the same masks.
            (model.tools as? RustDevelopTools)?.maskProvider = develop.maskProvider
            model.wirePersonMasks(develop: develop) // per-person people masks (renders, thumbnails, export)
            model.wireDepth() // lens blur depth maps (renders, thumbnails, export)
            // Removals: the fill cache; LaMa when already downloaded. The photo on screen
            // re-renders when LaMa takes over.
            RemovalModel.shared.onEngineChange = { [weak model] in model?.removalEngineChanged() }
            RemovalModel.shared.start()
            // Natural-language search: MobileCLIP (Core ML) + ds-session vectors.
            model.search.configure(backend: RustSearchBackend(store: store), models: .shared)
            // People (Vision faces, on device) and Find across sessions.
            model.people.configure(backend: RustPeopleBackend(store: store))
            model.find.configure(backend: RustFindBackend())
            model.map.configure(source: RustMapPointSource()) // All Sessions
            // [library] Every session's photos: developed thumbnails from the disk cache, else embedded previews.
            model.library.configure(backend: RustLibraryBackend(store: store, previews: previews) { url, variant, hash in
                await tools.cachedThumbnail(url: url, variant: variant, editHash: hash)
            })
            // HDR and panorama merges (ds-merge): the new photo is registered in the open session.
            model.merger = RustPhotoMerger(handles: { await store.archiveHandle(for: $0) })
            // AI Optimize: rules in ds-pipeline on the shared renderer; Vision (and the sky
            // model, when installed) for the scene.
            model.optimizer = RustPhotoOptimizer()
            model.optimizeMasks = develop.maskProvider
            model.optimizeSignals = { [weak model] photo in
                guard let model else { return OptimizeSignals() }
                return await model.visionOptimizeSignals(
                    photo, aiDenoiseAvailable: DenoiseService.shared.model.availability == .available,
                    skyModelReady: SkySegmentation.shared.state == .ready)
            }
            return model
        case .demo:
            let store = MockSessionStore()
            let model = AppModel(
                sessions: store,
                images: MockImageProvider(),
                renderer: RustRenderer(),
                importer: MockImportService(),
                archive: MockArchiveService(target: settings.archiveTarget.isEmpty ? "/Volumes/demo/Darkslide" : settings.archiveTarget),
                ai: makeAIService(),
                settings: settings,
                mode: .demo)
            model.merger = MockPhotoMerger(store: store)
            return model
        }
    }

    public static func live() -> AppModel {
        CameraProfileLibrary.startLoading()
        let model = make(mode: mode(arguments: CommandLine.arguments), settings: AppSettings(defaults: appDefaults),
                         defaults: appDefaults)
        // Voice commands: on-device speech; nothing is asked or opened until the user turns them on.
        model.voice.engine = SpeechAnalyzerEngine()
        model.voice.permissions = SystemVoicePermissions()
        if model.mode == .live {
            // The rawler helper, when the user installed it (Settings ▸ RAW): before the
            // first photo is decoded, so files that need it develop from the start.
            RawlerHelper.shared.activate()
            // AI Denoise: Core ML on this Mac; an export denoises a photo just before it renders.
            // Process-wide (the Rust store), so only the app wires it, never a test's live model.
            DenoiseService.shared.app = model
            model.denoiser = DenoiseService.shared // closing a session stops its queued photos
            DenoiseService.shared.configure()
            AppDenoiseProvider.install { try await DenoiseService.shared.ensureForExport($0) }
            // AI Upscale: Photo ▸ Enhance ▸ Upscale 2×, and export recipes that enlarge with it.
            UpscaleService.shared.app = model
            model.upscaler = UpscaleService.shared
            AppUpscaleProvider.install { try await UpscaleService.shared.upscaleForExport($0) }
        }
        return model
    }

    /// `--snapshot` runs keep their preferences and recent folders in their own
    /// suite, never in the user's app domain.
    public static let snapshotSuite = "casa.baillargeon.darkslide.snapshot"

    /// The preferences every part of the app reads and writes (settings, recents,
    /// offered cards, tool layout, export recipes). The user's domain normally; a
    /// separate suite for `--defaults-suite <name>`, for dev runs with
    /// `DS_SUPPORT_ROOT` (`<root>/preferences.plist`), and for `--snapshot` runs.
    public static let appDefaults: UserDefaults =
        defaults(arguments: CommandLine.arguments, environment: ProcessInfo.processInfo.environment)

    /// Whether this process keeps its preferences out of the user's domain.
    public static var usesPrivateDefaults: Bool {
        defaultsSuiteName(arguments: CommandLine.arguments, environment: ProcessInfo.processInfo.environment) != nil
    }

    static func defaultsSuiteName(arguments: [String], environment: [String: String]) -> String? {
        if let i = arguments.firstIndex(of: "--defaults-suite"), i + 1 < arguments.count, !arguments[i + 1].isEmpty {
            return arguments[i + 1]
        }
        if let root = environment["DS_SUPPORT_ROOT"], !root.isEmpty {
            // A path suite: the plist lives in the support root, not in ~/Library/Preferences.
            return URL(fileURLWithPath: root, isDirectory: true).appendingPathComponent("preferences").path
        }
        return arguments.contains("--snapshot") ? snapshotSuite : nil
    }

    static func defaults(arguments: [String], environment: [String: String] = [:]) -> UserDefaults {
        let name = defaultsSuiteName(arguments: arguments, environment: environment)
        // Every `--snapshot` run starts from empty preferences, so one run's
        // archive target / Amazon folder never leaks into the next.
        // Only a private suite is emptied: `--defaults-suite com.apple.finder --snapshot` must not wipe that domain.
        if arguments.contains("--snapshot"), let name, name == snapshotSuite || name.hasPrefix("/") {
            resetSnapshotDefaults(name)
        }
        return name.flatMap(UserDefaults.init(suiteName:)) ?? .standard
    }

    /// Empty the preferences suite `suite` (keys and plist). Only ever called
    /// for a private suite, never the user's app domain.
    public static func resetSnapshotDefaults(_ suite: String) {
        guard suite != Bundle.main.bundleIdentifier, suite != "casa.baillargeon.darkslide",
              let d = UserDefaults(suiteName: suite) else { return }
        d.removePersistentDomain(forName: suite)
        for key in d.dictionaryRepresentation().keys where d.object(forKey: key) != nil {
            d.removeObject(forKey: key)
        }
    }
}
