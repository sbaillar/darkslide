#if os(macOS)
import ColorSync
#endif
import DarkslideFFI
import Foundation
import Observation
import simd

/// An installed ICC profile the viewer can proof against.
public struct ProofProfile: Codable, Equatable, Hashable, Identifiable, Sendable {
    /// Display and colour-space profiles (sRGB, Adobe RGB are display-class), or output
    /// (printer and paper) profiles.
    public enum Kind: String, Codable, Sendable, Comparable {
        case display, printer

        public static func < (a: Kind, b: Kind) -> Bool { a == .display && b == .printer }

        public var title: String { self == .display ? "Screens and color spaces" : "Printers and papers" }
    }

    public enum Model: String, Codable, Sendable { case rgb, cmyk, gray }

    public var name: String
    /// Absolute path of the `.icc` / `.icm` file.
    public var path: String
    public var kind: Kind
    public var model: Model

    public var id: String { path }
    /// Export embeds RGB profiles only (a CMYK or gray file isn't what a recipe writes).
    public var exportable: Bool { model == .rgb }

    public init(name: String, path: String, kind: Kind, model: Model) {
        self.name = name
        self.path = path
        self.kind = kind
        self.model = model
    }
}

/// Rendering intent of the proof (and of an export to the proof profile).
public enum ProofIntent: String, Codable, CaseIterable, Sendable {
    case perceptual, relativeColorimetric

    public var title: String { self == .perceptual ? "Perceptual" : "Relative colorimetric" }

    var ffi: ProofRenderingIntent { self == .perceptual ? .perceptual : .relativeColorimetric }
}

/// The colour out-of-gamut pixels are painted in.
public enum GamutWarningColor: String, Codable, CaseIterable, Sendable {
    case gray, magenta, green, cyan, red

    public var title: String { rawValue.capitalized }

    /// Linear Display P3.
    public var linearP3: SIMD3<Float> {
        switch self {
        case .gray: SIMD3(0.5, 0.5, 0.5)
        case .magenta: SIMD3(1, 0, 1)
        case .green: SIMD3(0, 1, 0)
        case .cyan: SIMD3(0, 1, 1)
        case .red: SIMD3(1, 0, 0)
        }
    }
}

/// What soft proofing remembers (Settings suite, JSON). The on/off state is not
/// remembered: the app always starts without a proof.
public struct SoftProofSettings: Codable, Equatable, Sendable {
    public var profile: ProofProfile?
    public var intent: ProofIntent = .perceptual
    public var blackPointCompensation = true
    public var gamutWarning = false
    public var warningColor: GamutWarningColor = .gray

    public init() {}
}

/// Saved soft-proof settings in the app's suite (`AppSettings.userDefaults`), so tests
/// and `--snapshot` runs never touch the real preferences.
public struct SoftProofStore {
    public static let key = "softProof"
    let defaults: UserDefaults

    public init(defaults: UserDefaults) { self.defaults = defaults }

    public func load() -> SoftProofSettings {
        defaults.data(forKey: Self.key).flatMap { try? JSONDecoder().decode(SoftProofSettings.self, from: $0) }
            ?? SoftProofSettings()
    }

    public func save(_ s: SoftProofSettings) {
        if let data = try? JSONEncoder().encode(s) { defaults.set(data, forKey: Self.key) }
    }
}

/// Lists the installed profiles (seam: ColorSync live, a fixed list in tests).
public protocol ProofProfileCatalog: Sendable {
    func profiles() -> [ProofProfile]
}

/// A proof table ready for the viewer (see `ds_export::proof`): `size`³ RGBA, red
/// fastest; sRGB-encoded Rec.2020 in, linear Rec.2020 + signed gamut distance (> 0:
/// outside) out.
public struct ProofTable: Equatable, Sendable {
    /// Unique per built table (the viewer caches its texture by it).
    public let id: UInt64
    public let size: Int
    public let rgba: [Float]
    public let profileName: String
    /// The profile has rendering-intent tables (false: perceptual = relative colorimetric).
    public let hasIntents: Bool
    /// Gray profiles: out of gamut is encoded chroma above this (see ds-export).
    public let neutralTolerance: Float?

    private nonisolated(unsafe) static var next: UInt64 = 0
    private static let lock = NSLock()

    public init(size: Int, rgba: [Float], profileName: String, hasIntents: Bool = true,
                neutralTolerance: Float? = nil) {
        id = Self.lock.withLock { Self.next += 1; return Self.next }
        self.size = size
        self.rgba = rgba
        self.profileName = profileName
        self.hasIntents = hasIntents
        self.neutralTolerance = neutralTolerance
    }
}

/// Builds proof tables (seam: Rust/Little CMS live, a fake in tests). Blocking.
public protocol ProofTableBuilder: Sendable {
    func table(profilePath: String, intent: ProofIntent, blackPointCompensation: Bool) throws -> ProofTable
}

/// The live builder: `ds_ffi::proof_lut`.
public struct RustProofTableBuilder: ProofTableBuilder {
    public init() {}

    public func table(profilePath: String, intent: ProofIntent, blackPointCompensation: Bool) throws -> ProofTable {
        let t = try proofLut(profilePath: profilePath, intent: intent.ffi, blackPointCompensation: blackPointCompensation)
        return ProofTable(size: Int(t.size), rgba: t.rgba, profileName: t.profileName, hasIntents: t.hasIntents,
                          neutralTolerance: t.neutralTolerance)
    }
}

/// What the viewer draws with the proof on.
public struct ProofDisplay: Equatable, Sendable {
    public var tableID: UInt64
    public var gamutWarning: Bool
    /// Linear Display P3.
    public var warningColor: SIMD3<Float>
    /// Gray profiles' chroma limit (nil: the table's alpha).
    public var neutralTolerance: Float?
}

/// Soft proofing (View ▸ Soft Proof, ⇧S): a profile, an intent, black point
/// compensation and a gamut warning; the proof table is built off the main thread
/// whenever one of them changes while the proof is on. One build runs at a time: a
/// change during a build is picked up when it lands (intermediate settings are never
/// built), and the previous table stays on screen until the new one replaces it.
@MainActor @Observable
public final class SoftProofModel {
    @ObservationIgnored let store: SoftProofStore
    @ObservationIgnored var catalog: any ProofProfileCatalog
    @ObservationIgnored var builder: any ProofTableBuilder
    /// Something the viewer draws changed (the app bumps the viewer's display version).
    @ObservationIgnored public var onDisplayChange: (() -> Void)?

    public private(set) var settings: SoftProofSettings
    public private(set) var enabled = false
    /// Installed profiles, sorted by kind then name (scanned off the main thread when
    /// the proof is first turned on).
    public private(set) var profiles: [ProofProfile] = []
    /// The latest table built (nil while off, before the first build lands, or after
    /// a failure). It may be for the previous settings while a rebuild runs.
    public private(set) var table: ProofTable?
    /// Why the last table couldn't be built (nil: fine).
    public private(set) var error: String?
    /// The settings the running build is for (nil: none running).
    @ObservationIgnored private var building: BuildKey?
    @ObservationIgnored private(set) var buildTask: Task<Void, Never>?
    @ObservationIgnored private(set) var scanTask: Task<Void, Never>?

    private struct BuildKey: Equatable {
        var path: String
        var intent: ProofIntent
        var blackPointCompensation: Bool
    }

    public init(store: SoftProofStore, catalog: any ProofProfileCatalog, builder: any ProofTableBuilder) {
        self.store = store
        self.catalog = catalog
        self.builder = builder
        settings = store.load()
    }

    /// The proof's profile and intent for an export recipe (RGB profiles only).
    public var recipeProfile: RecipeOutputProfile? {
        guard let p = settings.profile, p.exportable else { return nil }
        return RecipeOutputProfile(name: p.name, path: p.path, intent: settings.intent,
                                   blackPointCompensation: settings.blackPointCompensation)
    }

    /// Whether the intent choice changes anything for the current profile (matrix/TRC
    /// profiles have no intent tables). True until a table says otherwise.
    public var intentsApply: Bool { table?.hasIntents ?? true }

    public var display: ProofDisplay? {
        guard enabled, let table else { return nil }
        return ProofDisplay(tableID: table.id, gamutWarning: settings.gamutWarning,
                            warningColor: settings.warningColor.linearP3, neutralTolerance: table.neutralTolerance)
    }

    /// Short status for the viewer and toasts, e.g. "Proof: Adobe RGB (1998)": the
    /// profile whose table is drawn, which during a rebuild is still the previous one.
    public var statusText: String? {
        guard enabled else { return nil }
        if let error { return "Proof failed: \(error)" }
        if let table { return "Proof: \(table.profileName)" }
        return settings.profile.map { "Proof: preparing \($0.name)…" }
    }

    /// The profiles grouped for the picker (color spaces, displays, printers).
    public var profileGroups: [(kind: ProofProfile.Kind, profiles: [ProofProfile])] {
        Dictionary(grouping: profiles, by: \.kind).sorted { $0.key < $1.key }.map { ($0.key, $0.value) }
    }

    /// Scan the installed profiles off the main thread (once; a printer driver's packs
    /// can hold hundreds).
    public func loadProfiles() async {
        if profiles.isEmpty, scanTask == nil {
            let catalog = catalog
            scanTask = Task { [weak self] in
                let found = await Task.detached(priority: .userInitiated) { catalog.profiles() }.value
                self?.profiles = found.sorted {
                    ($0.kind, $0.name.localizedLowercase) < ($1.kind, $1.name.localizedLowercase)
                }
            }
        }
        await scanTask?.value
    }

    /// Turn the proof on or off. With no profile chosen yet, sRGB (else the first) is used.
    public func toggle() { setEnabled(!enabled) }

    public func setEnabled(_ on: Bool) {
        guard on != enabled else { return }
        enabled = on
        onDisplayChange?()
        guard on else {
            table = nil
            error = nil
            return
        }
        Task {
            await loadProfiles()
            guard enabled else { return }
            if settings.profile == nil {
                settings.profile = profiles.first { $0.name.localizedCaseInsensitiveContains("sRGB") } ?? profiles.first
                store.save(settings)
            }
            rebuild()
        }
    }

    public func setProfile(_ p: ProofProfile?) { update { $0.profile = p } }
    public func setIntent(_ i: ProofIntent) { update { $0.intent = i } }
    public func setBlackPointCompensation(_ on: Bool) { update { $0.blackPointCompensation = on } }

    public func setGamutWarning(_ on: Bool) {
        settings.gamutWarning = on
        store.save(settings)
        onDisplayChange?()
    }

    public func setWarningColor(_ c: GamutWarningColor) {
        settings.warningColor = c
        store.save(settings)
        onDisplayChange?()
    }

    /// A change the table depends on: saved, then rebuilt when the proof is on.
    private func update(_ change: (inout SoftProofSettings) -> Void) {
        var s = settings
        change(&s)
        guard s != settings else { return }
        settings = s
        store.save(s)
        if enabled { rebuild() }
    }

    private var currentKey: BuildKey? {
        settings.profile.map { BuildKey(path: $0.path, intent: settings.intent,
                                        blackPointCompensation: settings.blackPointCompensation) }
    }

    /// Build a table for the current settings, unless one is running (it rebuilds for
    /// the latest settings when it lands).
    private func rebuild() {
        guard building == nil else { return }
        guard let key = currentKey else {
            table = nil
            error = "No profile chosen"
            onDisplayChange?()
            return
        }
        building = key
        let builder = builder
        buildTask = Task { [weak self] in
            let result = await Task.detached(priority: .userInitiated) {
                Result { try builder.table(profilePath: key.path, intent: key.intent,
                                           blackPointCompensation: key.blackPointCompensation) }
            }.value
            guard let self else { return }
            building = nil
            guard enabled else { return }
            guard key == currentKey else { return rebuild() } // superseded: build the latest
            switch result {
            case .success(let t):
                table = t
                error = nil
            case .failure(let e):
                table = nil
                error = Self.plain(e)
            }
            onDisplayChange?()
        }
    }

    /// A build failure as the user reads it ("not a readable ICC profile").
    static func plain(_ e: Error) -> String {
        let m = if case DsError.InvalidArgument(let message) = e { message } else { "\(e)" }
        return m.hasPrefix("ICC profile: ") ? String(m.dropFirst("ICC profile: ".count)) : m
    }
}

/// Installed profiles from ColorSync (system, local, user and printer-driver
/// folders): display, colour space and printer profiles in RGB, CMYK or gray.
public struct ColorSyncProfileCatalog: ProofProfileCatalog {
    public init() {}

    public func profiles() -> [ProofProfile] {
        #if os(macOS)
        final class Box { var items: [ProofProfile] = [] }
        let box = Box()
        let info = Unmanaged.passUnretained(box).toOpaque()
        ColorSyncIterateInstalledProfiles({ dict, info in
            guard let dict = (dict as NSDictionary?) as? [String: Any], let info else { return true }
            let box = Unmanaged<Box>.fromOpaque(info).takeUnretainedValue()
            if let p = ColorSyncProfileCatalog.profile(dict) { box.items.append(p) }
            return true
        }, nil, info, nil)
        var seen = Set<String>()
        return box.items.filter { seen.insert($0.path).inserted }
        #else
        return [] // iOS (unverified): no installed-profile iteration; profiles are imported (phase 3)
        #endif
    }

    /// One iteration entry → a profile, or nil for kinds the viewer can't proof against.
    static func profile(_ d: [String: Any]) -> ProofProfile? {
        // The values of kColorSyncProfileURL etc. (C globals Swift 6 won't read).
        let url = d["com.apple.ColorSync.ProfileURL"] as? URL
        let cls = d["com.apple.ColorSync.ProfileClass"] as? String
        let space = d["com.apple.ColorSync.ProfileColorSpace"] as? String
        let desc = d["com.apple.ColorSync.ProfileDescription"] as? String
        return profile(path: url?.path, classSig: cls, spaceSig: space, description: desc)
    }

    static func profile(path: String?, classSig: String?, spaceSig: String?, description: String?) -> ProofProfile? {
        guard let path else { return nil }
        let kind: ProofProfile.Kind
        switch classSig?.trimmingCharacters(in: .whitespaces) {
        case "mntr", "spac": kind = .display
        case "prtr": kind = .printer
        default: return nil
        }
        let model: ProofProfile.Model
        switch spaceSig?.trimmingCharacters(in: .whitespaces) {
        case "RGB": model = .rgb
        case "CMYK": model = .cmyk
        case "GRAY": model = .gray
        default: return nil
        }
        let name = description.flatMap { $0.isEmpty ? nil : $0 }
            ?? URL(fileURLWithPath: path).deletingPathExtension().lastPathComponent
        return ProofProfile(name: name, path: path, kind: kind, model: model)
    }
}
