import Foundation
import Observation

/// File ▸ Import Capture One Settings…: read the folder (nothing written), show what
/// each category holds and what the import would do, then import the ticked
/// categories. Revert takes back every settings import since the last revert.
@MainActor @Observable
public final class C1SettingsImportModel {
    public enum Phase: Equatable, Sendable {
        case scanning
        case preview(C1SettingsPlan)
        case importing
        case done(C1SettingsPlan)
        case reverted(C1SettingsRevert)
        case failed(String)
    }

    public private(set) var phase: Phase = .scanning
    public private(set) var folder: URL
    public private(set) var preferences: URL?
    /// The ticked categories.
    public var categories = Set(C1SettingsCategory.allCases)
    public private(set) var scan: C1SettingsScan?
    /// An earlier settings import can be taken back.
    public private(set) var canRevert = false
    @ObservationIgnored let reader: any C1SettingsReading
    @ObservationIgnored let importer: C1SettingsImporter

    public init(folder: URL, preferences: URL?, reader: any C1SettingsReading, importer: C1SettingsImporter) {
        self.folder = folder
        self.preferences = preferences
        self.reader = reader
        self.importer = importer
        canRevert = importer.canRevert
    }

    public static let revertHint = "Revert takes back every Capture One settings import since the last revert. "
        + "Anything you changed in Darkslide afterwards is kept."

    /// The plan shown before importing.
    public var preview: C1SettingsPlan? {
        if case .preview(let p) = phase { return p }
        return nil
    }

    /// The ticked categories hold something to write.
    public var canImport: Bool { (preview?.changes(in: categories) ?? 0) > 0 }

    /// Read the folder and plan (the dry run: nothing is written).
    public func load() async {
        phase = .scanning
        do {
            let s = try await reader.read(folder: folder, preferences: preferences)
            scan = s
            phase = .preview(importer.plan(s))
            categories = Set(C1SettingsCategory.allCases)
        } catch is CancellationError {
            phase = .failed("Cancelled")
        } catch {
            phase = .failed(error.localizedDescription)
        }
        canRevert = importer.canRevert
    }

    /// Read another folder (Choose Folder…). Its preferences are not the app's own.
    public func choose(folder: URL) async {
        self.folder = folder
        preferences = nil
        await load()
    }

    /// Import the ticked categories (a fresh plan, so a store changed meanwhile counts).
    public func run() async {
        guard canImport, let scan else { return }
        let plan = importer.plan(scan)
        phase = .importing
        do {
            phase = .done(try importer.apply(plan, categories: categories))
        } catch {
            phase = .failed(error.localizedDescription)
        }
        canRevert = importer.canRevert
    }

    public func revert() {
        guard canRevert else { return }
        do {
            phase = .reverted(try importer.revert())
        } catch {
            phase = .failed(error.localizedDescription)
        }
        canRevert = importer.canRevert
    }

    /// One line per ticked category after an import: "Export recipes: 2 new, 1 up to date".
    public func doneLines(_ plan: C1SettingsPlan) -> [String] {
        C1SettingsCategory.allCases.filter { categories.contains($0) && !plan.rows(in: $0).isEmpty }
            .map { "\($0.title): \(plan.summary($0))" }
    }
}

extension C1SettingsImportModel: Equatable {
    public nonisolated static func == (a: C1SettingsImportModel, b: C1SettingsImportModel) -> Bool { a === b }
}

extension AppModel {
    /// The import's targets: the app's own stores.
    func c1SettingsImporter() -> C1SettingsImporter {
        let targets = C1SettingsStores(recipes: ExportRecipeStore(defaults: settings.userDefaults), layout: .shared,
                                       presets: .shared, ratios: .shared, keywords: .shared, settings: settings,
                                       softProof: softProof)
        return C1SettingsImporter(targets: targets, journalURL: AppPaths.support.appendingPathComponent(C1SettingsJournal.fileName))
    }

    /// File ▸ Import Capture One Settings…: show the sheet and read the default folder.
    public func showCaptureOneSettingsImport() {
        let loc = RustC1SettingsReader.defaultLocations()
        let m = C1SettingsImportModel(folder: loc.folder, preferences: loc.preferences, reader: c1SettingsReader,
                                      importer: c1SettingsImporter())
        sheet = .captureOneSettings(m)
        Task { await m.load() }
    }
}
