import CryptoKit
import DarkslideFFI
import Foundation

/// A shareable benchmark result (`bench/schema/bench-result-v1.schema.json`): what a
/// future public results browser would take. It holds the app versions, the Mac's
/// model, the cameras, the test pack, the settings and per-operation summaries.
///
/// It never holds serial numbers, file names, paths, GPS, user or host names, or
/// photos. Every field is built from a whitelist (never copied from free text), and
/// `privacyProblems` checks the result again before it is written.
public struct BenchShare: Codable, Equatable, Sendable {
    public static let schemaName = "darkslide-bench-result"
    public static let currentVersion = 1

    public struct Machine: Codable, Equatable, Sendable {
        /// e.g. `Mac16,8`.
        public var modelID: String
        /// e.g. `Apple M4 Pro`.
        public var chip: String
        public var cpuCores: Int
        public var performanceCores: Int
        public var efficiencyCores: Int
        public var memoryGB: Int
        public var macOS: String
        public var macOSBuild: String
    }

    public struct Camera: Codable, Equatable, Sendable {
        public var make: String
        public var model: String
        public var megapixels: Double
        /// Distinct files of this camera in the set.
        public var files: Int
    }

    public struct TestPack: Codable, Equatable, Sendable {
        /// `standard-v1`, or `custom` for the user's own photos.
        public var id: String
        /// SHA-256 over the sorted SHA-256 digests of the distinct files (`contentHash`).
        public var hash: String
        /// Only a verified standard pack with enough photos is ranked.
        public var ranked: Bool
    }

    public struct Settings: Codable, Equatable, Sendable {
        public var photos: Int
        public var runs: Int
        public var fileCachePurged: Bool
        public var importMode: String
        public var gpu: Bool
        public var exportFormat: String
        public var exportSize: String
        public var exportColorSpace: String
        public var jpegQuality: Int
        public var outputSharpening: Bool
        public var selectDwellMs: Int
        public var pressIntervalMs: Int
        public var settleMs: Int
    }

    public struct Operation: Codable, Equatable, Sendable {
        public var operation: BenchOperation
        public var mode: BenchRecord.Mode
        public var method: String
        /// Comparable across apps (viewer operations timed on screen, batch operations by
        /// wall clock). In-app viewer timings are kept for the app's own history only.
        public var comparable: Bool
        public var n: Int
        public var missed: Int
        public var medianMs: Double
        public var p90Ms: Double
        /// Batch operations: the median per photo.
        public var perPhotoMs: Double?
    }

    public struct AppResult: Codable, Equatable, Sendable {
        public var name: String
        public var version: String
        public var build: String
        /// Long edge of the previews this app built at import (what "import done" waited for).
        public var previewSizePx: Int?
        public var operations: [Operation]
    }

    public var schema: String
    public var schemaVersion: Int
    /// The day of the run (no time of day).
    public var date: String
    public var machine: Machine
    public var cameras: [Camera]
    public var testPack: TestPack
    public var settings: Settings
    /// Apps that ran (an app that wasn't run is left out, not ranked as zero).
    public var apps: [AppResult]

    // MARK: - Building

    /// The result from a results folder's records and meta, the machine, the photo set
    /// and the pack. Only structured fields are taken; free text in `meta` is not.
    public static func make(records: [BenchRecord], meta: BenchMeta, machine: Machine, cameras: [Camera],
                            testPack: TestPack, settings: Settings, date: Date) -> BenchShare {
        let day = ISO8601DateFormatter.string(from: date, timeZone: .current, formatOptions: [.withFullDate])
        let names = BenchReport.appOrder(records, meta: meta).filter { BenchReport.ran($0, records: records, meta: meta) }
        let apps = names.map { name -> AppResult in
            let m = meta.apps.first { $0.name == name }
            var ops: [Operation] = []
            for op in BenchOperation.allCases {
                for mode in [BenchRecord.Mode.cold, .warm] {
                    // One entry per timing method: samples of different methods are never pooled.
                    for method in BenchReport.methods(records, app: name, op: op, mode: mode) {
                        let subset = records.filter {
                            $0.app == name && $0.operation == op && $0.mode == mode && $0.method == method
                        }
                        guard let s = BenchSummary.of(subset.map(\.ms)) else { continue }
                        let items = subset.map(\.items).max() ?? 1
                        ops.append(Operation(operation: op, mode: mode, method: method, comparable: op.comparable(method: method),
                                             n: s.n, missed: s.missed, medianMs: rounded(s.median), p90Ms: rounded(s.p90),
                                             perPhotoMs: items > 1 ? rounded(s.median / Double(items)) : nil))
                    }
                }
            }
            let px = records.filter { $0.app == name && $0.operation == .importFiles }.compactMap(\.previewPx).sorted()
            return AppResult(name: name, version: m?.version ?? "", build: m?.build ?? "",
                             previewSizePx: px.isEmpty ? nil : px[px.count / 2], operations: ops)
        }
        return BenchShare(schema: schemaName, schemaVersion: currentVersion, date: day, machine: machine,
                          cameras: cameras, testPack: testPack, settings: settings, apps: apps)
    }

    static func rounded(_ ms: Double) -> Double { (ms * 10).rounded() / 10 }

    /// The kit's fixed settings, with what the run recorded in `meta`. Without a
    /// `meta.json` (a run-darkslide.sh run alone), photos and runs come from the records.
    public static func settings(meta: BenchMeta, records: [BenchRecord] = []) -> Settings {
        let o = DarkslideBench.Options(source: URL(fileURLWithPath: "/"), work: URL(fileURLWithPath: "/"))
        let photos = meta.photos > 0 ? meta.photos : records.filter { $0.operation == .importFiles }.map(\.items).max() ?? 0
        let runs = meta.runs ?? Set(records.map { "\($0.app)\u{1}\($0.run)" }).count
        return Settings(photos: photos, runs: runs, fileCachePurged: meta.fileCachePurged ?? false,
                        importMode: "in-place", gpu: true, exportFormat: "jpeg",
                        exportSize: "original", exportColorSpace: "sRGB", jpegQuality: meta.jpegQuality ?? o.jpegQuality,
                        outputSharpening: false, selectDwellMs: Int(o.selectDwell * 1000),
                        pressIntervalMs: Int(o.pressInterval * 1000), settleMs: Int(FrameDiff().settle * 1000))
    }

    /// The pack the photos are: the manifest's when their content hash matches it (ranked
    /// when the run had enough photos), else `custom` (never ranked).
    public static func testPack(hash: String, manifest: BenchPack?, photos: Int) -> TestPack {
        if let manifest, manifest.hash == hash {
            return TestPack(id: manifest.id, hash: hash, ranked: photos >= manifest.minimumPhotos)
        }
        return TestPack(id: "custom", hash: hash, ranked: false)
    }

    // MARK: - Photo set

    /// SHA-256 over the sorted, distinct SHA-256 digests of `files` (hex, one per line):
    /// the same set of photos gives the same hash whatever their names or order.
    public static func contentHash(_ files: [URL]) throws -> String {
        let digests = Set(try files.map(fileSHA256)).sorted()
        let joined = digests.map { $0 + "\n" }.joined()
        return SHA256.hash(data: Data(joined.utf8)).map { String(format: "%02x", $0) }.joined()
    }

    static func fileSHA256(_ url: URL) throws -> String {
        let h = try FileHandle(forReadingFrom: url)
        defer { try? h.close() }
        var d = SHA256()
        while let chunk = try h.read(upToCount: 1 << 22), !chunk.isEmpty { d.update(data: chunk) }
        return d.finalize().map { String(format: "%02x", $0) }.joined()
    }

    /// Cameras in `files` (make, model, megapixels from each file's metadata; distinct
    /// files counted once by content). Nothing else from the metadata is kept.
    public static func cameras(_ files: [URL]) async throws -> [Camera] {
        var seen = Set<String>()
        var counts: [String: Camera] = [:]
        for f in files {
            guard seen.insert(try fileSHA256(f)).inserted else { continue }
            let m = try await rawMetadata(path: f.path, token: try CancelToken())
            let mp = (Double(m.width ?? 0) * Double(m.height ?? 0) / 1e6 * 10).rounded() / 10
            let make = m.make.trimmingCharacters(in: .whitespaces)
            let model = m.model.trimmingCharacters(in: .whitespaces)
            let key = "\(make)\u{1}\(model)\u{1}\(mp)"
            counts[key, default: Camera(make: make, model: model, megapixels: mp, files: 0)].files += 1
        }
        return counts.values.sorted { ($0.make, $0.model, $0.megapixels) < ($1.make, $1.model, $1.megapixels) }
    }

    // MARK: - Privacy

    /// What must never appear in a shared result, for this user and Mac.
    public struct PrivateWords: Sendable {
        public var words: [String]

        public init(words: [String]) {
            self.words = words.map { $0.trimmingCharacters(in: .whitespaces) }.filter { $0.count >= 3 }
        }

        public static var current: PrivateWords {
            let host = ProcessInfo.processInfo.hostName
            // The full name and each part of it ("Lovelace" alone too), at 4+ letters.
            let parts = NSFullUserName().components(separatedBy: .whitespaces).filter { $0.count >= 4 }
            return PrivateWords(words: [NSUserName(), NSFullUserName(), NSHomeDirectory(), host,
                                        host.components(separatedBy: ".").first ?? ""] + parts)
        }
    }

    static let fileExtension = #"\.(arw|cr2|cr3|crw|nef|nrw|raf|rw2|orf|dng|pef|srw|3fr|iiq|erf|rwl|jpe?g|heic|tiff?|png|xmp)\b"#

    /// Why `share` may not be shared (empty: fine). Every string in it is checked for
    /// paths, file names, e-mail addresses, the user's and the Mac's names, and length.
    public static func privacyProblems(_ share: BenchShare, private words: PrivateWords = .current) -> [String] {
        guard let data = try? JSONEncoder().encode(share),
              let json = try? JSONSerialization.jsonObject(with: data) else { return ["not encodable"] }
        var strings: [String] = []
        func walk(_ v: Any) {
            if let s = v as? String { strings.append(s) }
            if let a = v as? [Any] { a.forEach(walk) }
            if let d = v as? [String: Any] { d.values.forEach(walk) }
        }
        walk(json)
        var problems: [String] = []
        for s in strings {
            if s.contains("/") || s.contains("\\") || s.hasPrefix("~") { problems.append("a path: \(s.debugDescription)") }
            if s.contains("@") { problems.append("an address: \(s.debugDescription)") }
            if s.range(of: fileExtension, options: [.regularExpression, .caseInsensitive]) != nil {
                problems.append("a file name: \(s.debugDescription)")
            }
            if s.count > 80 { problems.append("too long: \(s.prefix(20))…") }
            for w in words.words where s.localizedCaseInsensitiveContains(w) {
                problems.append("a user or Mac name: \(s.debugDescription)")
            }
        }
        return problems
    }

    public func json() throws -> Data {
        let e = JSONEncoder()
        e.outputFormatting = [.prettyPrinted, .sortedKeys]
        return try e.encode(self)
    }
}

/// A test pack manifest (`bench/packs/<id>.json`): fixed, openly licensed RAWs fetched by
/// URL and pinned by hash (`bench/fetch-pack.sh`), never committed.
public struct BenchPack: Codable, Equatable, Sendable {
    public struct File: Codable, Equatable, Sendable {
        public var name: String
        public var url: String
        public var bytes: Int
        public var blake3: String
        public var sha256: String
        public var make: String
        public var model: String
        public var megapixels: Double
        public var licence: String
    }

    public var id: String
    public var schemaVersion: Int
    public var licence: String
    public var minimumPhotos: Int
    /// `BenchShare.contentHash` of the files.
    public var hash: String
    public var files: [File]

    public static func load(_ url: URL) throws -> BenchPack {
        try JSONDecoder().decode(BenchPack.self, from: Data(contentsOf: url))
    }

    /// The pack hash from the pinned per-file SHA-256 digests (must equal `hash`).
    public var expectedHash: String {
        let joined = Set(files.map(\.sha256)).sorted().map { $0 + "\n" }.joined()
        return SHA256.hash(data: Data(joined.utf8)).map { String(format: "%02x", $0) }.joined()
    }
}

extension BenchShare.Machine {
    /// This Mac, from sysctl. Never reads the serial number or any identifier of the unit.
    public static var current: BenchShare.Machine {
        let v = ProcessInfo.processInfo.operatingSystemVersion
        let os = v.patchVersion > 0 ? "\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)" : "\(v.majorVersion).\(v.minorVersion)"
        return BenchShare.Machine(
            modelID: sysctlString("hw.model"), chip: sysctlString("machdep.cpu.brand_string"),
            cpuCores: sysctlInt("hw.ncpu"), performanceCores: sysctlInt("hw.perflevel0.physicalcpu"),
            efficiencyCores: sysctlInt("hw.perflevel1.physicalcpu"),
            memoryGB: Int((Double(sysctlInt("hw.memsize")) / 1_073_741_824).rounded()),
            macOS: os, macOSBuild: sysctlString("kern.osversion"))
    }

    static func sysctlString(_ name: String) -> String {
        var size = 0
        guard sysctlbyname(name, nil, &size, nil, 0) == 0, size > 0 else { return "" }
        var buf = [CChar](repeating: 0, count: size)
        guard sysctlbyname(name, &buf, &size, nil, 0) == 0 else { return "" }
        return String(decoding: buf.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
    }

    static func sysctlInt(_ name: String) -> Int {
        var value: Int64 = 0
        var size = MemoryLayout<Int64>.size
        guard sysctlbyname(name, &value, &size, nil, 0) == 0 else { return 0 }
        // 4-byte values (hw.ncpu) fill the low half.
        return size == 4 ? Int(Int32(truncatingIfNeeded: value)) : Int(value)
    }
}
