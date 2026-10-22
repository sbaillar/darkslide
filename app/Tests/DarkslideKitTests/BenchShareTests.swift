import CryptoKit
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// The shareable result (`BenchShare`), the test pack manifest and the JSON schema
/// (docs/BENCHMARKS.md, Sharing results).
@Suite struct BenchShareTests {
    static let repo = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("../../..")
        .standardizedFileURL

    static let machine = BenchShare.Machine(modelID: "Mac16,8", chip: "Apple M4 Pro", cpuCores: 14, performanceCores: 10,
                                            efficiencyCores: 4, memoryGB: 48, macOS: "26.0", macOSBuild: "25A100")

    static func r(_ app: String, _ op: BenchOperation, _ ms: Double?, mode: BenchRecord.Mode = .cold, items: Int = 1,
                  method: String = "screen") -> BenchRecord {
        BenchRecord(app: app, operation: op, mode: mode, run: 1, sample: 1, ms: ms, items: items, method: method)
    }

    static func sample() -> BenchShare {
        var imported = r("Darkslide", .importFiles, 9_000, items: 150, method: "in-app")
        imported.previewPx = 1616
        let records = [10.0, 20, 30].map { r("Darkslide", .firstPreview, $0, method: "in-app") }
            + [r("Darkslide", .firstPreview, 35), imported]
            + [r("Darkslide", .export, 50_000, items: 100, method: "in-app"), r("Capture One", .nextPhoto, nil),
               r("Capture One", .nextPhoto, 44.44)]
        let meta = BenchMeta(photos: 150, apps: [.init(name: "Darkslide", version: "0.9.1", build: "812"),
                                                 .init(name: "Capture One", version: "16.0"),
                                                 .init(name: "Lightroom Classic", ran: false)],
                             runs: 3, fileCachePurged: true, jpegQuality: 90)
        return BenchShare.make(records: records, meta: meta, machine: machine,
                               cameras: [.init(make: "SONY", model: "ILCE-9", megapixels: 24.2, files: 8)],
                               testPack: .init(id: "custom", hash: String(repeating: "a", count: 64), ranked: false),
                               settings: BenchShare.settings(meta: meta), date: Date(timeIntervalSince1970: 1_790_000_000))
    }

    @Test func buildsPerAppOperationSummariesFromStructuredFieldsOnly() throws {
        let s = Self.sample()
        #expect(s.schema == "darkslide-bench-result" && s.schemaVersion == 1)
        #expect(s.date.count == 10 && s.date.hasPrefix("2026-09-"), "a day, no time: \(s.date)")
        #expect(s.apps.map(\.name) == ["Darkslide", "Capture One"], "Lightroom Classic didn't run: left out")
        let ds = try #require(s.apps.first)
        #expect(ds.version == "0.9.1" && ds.build == "812" && s.apps[1].build == "")
        // One entry per method, never pooled; in-app viewer numbers are marked not comparable.
        let fps = ds.operations.filter { $0.operation == .firstPreview }
        #expect(fps.map(\.method) == ["in-app", "screen"] && fps.map(\.comparable) == [false, true])
        let fp = try #require(fps.first)
        #expect(fp.n == 3 && fp.medianMs == 20 && fp.p90Ms == 30 && fp.missed == 0 && fp.perPhotoMs == nil)
        #expect(fps[1].n == 1 && fps[1].medianMs == 35)
        #expect(ds.previewSizePx == 1616 && s.apps[1].previewSizePx == nil, "each app's own preview size")
        let ex = try #require(ds.operations.first { $0.operation == .export })
        #expect(ex.perPhotoMs == 500 && ex.comparable)
        let np = try #require(s.apps[1].operations.first)
        #expect(np.n == 1 && np.missed == 1 && np.medianMs == 44.4, "rounded to 0.1 ms")
        #expect(s.settings == BenchShare.Settings(photos: 150, runs: 3, fileCachePurged: true, importMode: "in-place",
                                                  gpu: true, exportFormat: "jpeg",
                                                  exportSize: "original", exportColorSpace: "sRGB", jpegQuality: 90,
                                                  outputSharpening: false, selectDwellMs: 2000, pressIntervalMs: 100,
                                                  settleMs: 500))
        #expect(BenchShare.settings(meta: BenchMeta()).runs == 0 && BenchShare.settings(meta: BenchMeta()).jpegQuality == 90)
        // No meta.json: photos from the import records, runs from the distinct launches.
        let bare = BenchShare.settings(meta: BenchMeta(), records: [
            BenchRecord(app: "Darkslide", operation: .importFiles, mode: .cold, run: 1, sample: 1, ms: 9, items: 24, method: "in-app"),
            BenchRecord(app: "Darkslide", operation: .open, mode: .cold, run: 2, sample: 1, ms: 9, items: 24, method: "in-app"),
        ])
        #expect(bare.photos == 24 && bare.runs == 2)
    }

    /// Personal details in the free-text inputs (meta notes, preflight lines, file names in
    /// methods) never reach the shareable result.
    @Test func nothingPersonalLeaksFromTheInputs() throws {
        let secret = ["/Users/alice", "alice@example.com", "alice-macbook", "DSC01234.ARW", "SERIAL-4711", "GPS 52.37,4.89"]
        let meta = BenchMeta(date: "2026-10-10 14:03", machine: "alice-macbook (/Users/alice)", os: "SERIAL-4711",
                             photos: 150, camera: "DSC01234.ARW at GPS 52.37,4.89",
                             apps: [.init(name: "Darkslide", version: "1.0", build: "7", note: "alice@example.com")],
                             preflight: ["ok: files in /Users/alice/Pictures", "host alice-macbook"], runs: 3)
        let records = [Self.r("Darkslide", .exposure, 15, method: "in-app")]
        let share = BenchShare.make(records: records, meta: meta, machine: Self.machine,
                                    cameras: [.init(make: "SONY", model: "ILCE-9", megapixels: 24.2, files: 1)],
                                    testPack: .init(id: "custom", hash: String(repeating: "c", count: 64), ranked: false),
                                    settings: BenchShare.settings(meta: meta, records: records), date: Date())
        let text = String(decoding: try share.json(), as: UTF8.self)
        for s in secret { #expect(!text.contains(s), "\(s) leaked") }
        #expect(!text.contains("alice") && !text.contains("14:03"), "no names, no time of day")
        #expect(BenchShare.privacyProblems(share, private: .init(words: ["alice"])).isEmpty)
        // The live machine and user: none of their names in a real result either.
        let real = String(decoding: try Self.sample().json(), as: UTF8.self)
        for w in BenchShare.PrivateWords.current.words { #expect(!real.localizedCaseInsensitiveContains(w), "\(w)") }
    }

    @Test func onlyAVerifiedPackWithEnoughPhotosIsRanked() throws {
        let pack = try BenchPack.load(Self.repo.appendingPathComponent("bench/packs/standard-v1.json"))
        #expect(BenchShare.testPack(hash: pack.hash, manifest: pack, photos: 150)
            == .init(id: "standard-v1", hash: pack.hash, ranked: true))
        #expect(BenchShare.testPack(hash: pack.hash, manifest: pack, photos: 100).ranked == false, "too few photos")
        #expect(BenchShare.testPack(hash: "b", manifest: pack, photos: 150) == .init(id: "custom", hash: "b", ranked: false))
        #expect(BenchShare.testPack(hash: "b", manifest: nil, photos: 500).id == "custom")
    }

    @Test func theStandardPackIsPinnedOpenAndVaried() throws {
        let pack = try BenchPack.load(Self.repo.appendingPathComponent("bench/packs/standard-v1.json"))
        #expect(pack.id == "standard-v1" && pack.schemaVersion == 1 && pack.licence == "CC0-1.0")
        #expect(pack.expectedHash == pack.hash, "the pack hash is the hash of its pinned files")
        #expect(pack.minimumPhotos >= 121)
        #expect(Set(pack.files.map(\.make)).count >= 3 && pack.files.count >= 6, "several cameras")
        let hex = { (s: String) in s.count == 64 && s.allSatisfy(\.isHexDigit) }
        for f in pack.files {
            #expect(f.licence == "CC0-1.0", "\(f.name)")
            #expect(f.url.hasPrefix("https://raw.pixls.us/getfile.php/"), "\(f.name)")
            #expect(hex(f.blake3) && hex(f.sha256) && f.bytes > 1_000_000 && f.megapixels > 10, "\(f.name)")
            #expect(!f.name.contains("/") && !f.name.hasPrefix(".") && f.name == f.name.lowercased(), "\(f.name)")
        }
        #expect(Set(pack.files.map(\.name)).count == pack.files.count)
    }

    @Test func contentHashIgnoresNamesOrderAndCopies() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("bench-hash-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        func file(_ name: String, _ text: String) throws -> URL {
            let u = dir.appendingPathComponent(name)
            try Data(text.utf8).write(to: u)
            return u
        }
        let a = try file("a.arw", "one"), b = try file("b.arw", "two"), copy = try file("0001-a.arw", "one")
        let h = try BenchShare.contentHash([a, b])
        #expect(try BenchShare.contentHash([b, copy, a]) == h)
        #expect(try BenchShare.contentHash([a]) != h)
        // The same rule as a manifest's pinned per-file SHA-256 digests.
        let sha = { (s: String) in SHA256.hash(data: Data(s.utf8)).map { String(format: "%02x", $0) }.joined() }
        let f = { (d: String) in BenchPack.File(name: "x", url: "", bytes: 0, blake3: "", sha256: d, make: "", model: "",
                                                megapixels: 0, licence: "") }
        let manifest = BenchPack(id: "t", schemaVersion: 1, licence: "", minimumPhotos: 1, hash: "",
                                 files: [f(sha("two")), f(sha("one"))])
        #expect(manifest.expectedHash == h)
        #expect(throws: (any Error).self) { _ = try BenchShare.contentHash([dir.appendingPathComponent("nope")]) }
    }

    @Test func camerasFromTheFilesMetadata() async throws {
        let samples = sampleARWs()
        guard samples.count >= 2 else { return }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("bench-cams-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let copy = dir.appendingPathComponent("copy.ARW")
        try FileManager.default.copyItem(at: samples[0], to: copy)
        let cams = try await BenchShare.cameras([samples[0], samples[1], copy])
        #expect(cams.count == 1)
        let c = try #require(cams.first)
        #expect(c.model == "ILCE-9" && c.make.uppercased() == "SONY" && c.files == 2, "\(cams)")
        #expect(c.megapixels > 20 && c.megapixels < 30)
    }

    @Test func thisMacWithoutIdentifiers() {
        let m = BenchShare.Machine.current
        #expect(m.modelID.range(of: #"^[A-Za-z]+[0-9]+,[0-9]+$"#, options: .regularExpression) != nil, "\(m.modelID)")
        #expect(!m.chip.isEmpty && m.cpuCores > 0 && m.memoryGB > 0 && m.performanceCores > 0)
        #expect(m.macOS.range(of: #"^\d+\.\d+(\.\d+)?$"#, options: .regularExpression) != nil && !m.macOSBuild.isEmpty)
        #expect(BenchShare.Machine.sysctlString("no.such.key") == "" && BenchShare.Machine.sysctlInt("no.such.key") == 0)
    }

    @Test func privacyCheckCatchesPathsNamesAndIdentifiers() {
        let words = BenchShare.PrivateWords(words: ["alice", "studio-mac", "x"])
        #expect(words.words == ["alice", "studio-mac"], "too-short words are ignored")
        #expect(BenchShare.privacyProblems(Self.sample(), private: words).isEmpty)
        func with(_ model: String) -> [String] {
            var s = Self.sample()
            s.cameras[0].model = model
            return BenchShare.privacyProblems(s, private: words)
        }
        #expect(with("/Users/alice/Pictures").count == 2, "a path and a user name")
        #expect(with("C:\\photos").first?.hasPrefix("a path") == true)
        #expect(with("~photos").first?.hasPrefix("a path") == true)
        #expect(with("me@example.com").first?.hasPrefix("an address") == true)
        #expect(with("DSC01234.ARW").first?.hasPrefix("a file name") == true)
        #expect(with("shot.jpeg").first?.hasPrefix("a file name") == true)
        #expect(with("Studio-Mac").first?.hasPrefix("a user or Mac name") == true, "case-insensitive")
        #expect(with(String(repeating: "z", count: 81)).first?.hasPrefix("too long") == true)
        #expect(with("Z 6_2").isEmpty && with("EOS R5").isEmpty)
        #expect(!BenchShare.PrivateWords.current.words.isEmpty)
    }

    /// A small JSON Schema checker (the subset the schema uses).
    static func validate(_ v: Any, _ s: [String: Any], _ path: String = "$") -> [String] {
        var out: [String] = []
        let isBool = { (x: Any) in (x as? NSNumber).map { CFGetTypeID($0) == CFBooleanGetTypeID() } ?? false }
        switch s["type"] as? String {
        case "object":
            guard let o = v as? [String: Any] else { return ["\(path): not an object"] }
            let props = s["properties"] as? [String: [String: Any]] ?? [:]
            for k in s["required"] as? [String] ?? [] where o[k] == nil { out.append("\(path).\(k): missing") }
            if s["additionalProperties"] as? Bool == false {
                for k in o.keys where props[k] == nil { out.append("\(path).\(k): not allowed") }
            }
            for (k, sub) in props { if let x = o[k] { out += validate(x, sub, "\(path).\(k)") } }
        case "array":
            guard let a = v as? [Any] else { return ["\(path): not an array"] }
            for (i, x) in a.enumerated() { out += validate(x, s["items"] as? [String: Any] ?? [:], "\(path)[\(i)]") }
        case "string":
            guard let str = v as? String else { return ["\(path): not a string"] }
            if let e = s["enum"] as? [String], !e.contains(str) { out.append("\(path): \(str) not in enum") }
            if let c = s["const"] as? String, c != str { out.append("\(path): not \(c)") }
            if let p = s["pattern"] as? String, str.range(of: p, options: .regularExpression) == nil {
                out.append("\(path): \(str) doesn't match \(p)")
            }
            if let m = s["maxLength"] as? Int, str.count > m { out.append("\(path): too long") }
        case "integer", "number":
            guard let n = v as? NSNumber, !isBool(v) else { return ["\(path): not a number"] }
            if s["type"] as? String == "integer", n.doubleValue.rounded() != n.doubleValue { out.append("\(path): not an integer") }
            if let m = s["minimum"] as? Double, n.doubleValue < m { out.append("\(path): below \(m)") }
            if let m = s["maximum"] as? Double, n.doubleValue > m { out.append("\(path): above \(m)") }
            if let c = s["const"] as? Int, n.intValue != c { out.append("\(path): not \(c)") }
        case "boolean":
            if !isBool(v) { out.append("\(path): not a boolean") }
        default:
            out.append("\(path): unknown schema type")
        }
        return out
    }

    @Test func resultsMatchTheVersionedSchema() throws {
        let schema = try #require(try JSONSerialization.jsonObject(with: Data(contentsOf: Self.repo.appendingPathComponent(
            "bench/schema/bench-result-v1.schema.json"))) as? [String: Any])
        let json = try JSONSerialization.jsonObject(with: Self.sample().json())
        #expect(Self.validate(json, schema) == [])
        // The schema is strict: an extra field (a file name, say) is not allowed.
        var extra = try #require(json as? [String: Any])
        extra["files"] = ["a.ARW"]
        extra["date"] = "2026-09-21T10:00"
        #expect(Self.validate(extra, schema).sorted() == ["$.date: 2026-09-21T10:00 doesn't match ^\\d{4}-\\d{2}-\\d{2}$",
                                                          "$.files: not allowed"])
        #expect(Self.validate(true, ["type": "integer"]) == ["$: not a number"])
        #expect(Self.validate(1, ["type": "boolean"]) == ["$: not a boolean"])
    }

    @MainActor @Test func shareCommandWritesAPrivateResult() async throws {
        #expect(try BenchCommand.parse(["D", "--bench-share", "/r", "--photos", "/p", "--pack", "/m.json"])
            == .share(results: URL(fileURLWithPath: "/r"), photos: URL(fileURLWithPath: "/p"),
                      pack: URL(fileURLWithPath: "/m.json"), out: URL(fileURLWithPath: "/r/result.json")))
        #expect(try BenchCommand.parse(["D", "--bench-share", "/r", "--photos", "/p", "--out", "/x.json"])
            == .share(results: URL(fileURLWithPath: "/r"), photos: URL(fileURLWithPath: "/p"), pack: nil,
                      out: URL(fileURLWithPath: "/x.json")))
        #expect(BenchCommand.wants(["D", "--bench-share"]))
        #expect(BenchCommand.ShareError.privacy(["a", "b"]).description.hasSuffix("a; b"))

        let samples = sampleARWs()
        guard samples.count >= 2 else { return }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("bench-share-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        let results = dir.appendingPathComponent("results")
        try BenchResults.write(Data(BenchResults.csv([Self.r("Darkslide", .open, 12)]).utf8),
                               to: results.appendingPathComponent("ds.csv"))
        try BenchResults.write(try JSONEncoder().encode(BenchMeta(photos: 2, apps: [.init(name: "Darkslide")], runs: 1)),
                               to: results.appendingPathComponent("meta.json"))
        let photos = try DarkslideBench.freshFolder(Array(samples.prefix(2)), work: dir, name: "photos", tag: "s")
        // A pack whose pinned digests are these two files: the result is that pack, ranked.
        let digests = try samples.prefix(2).map(BenchShare.fileSHA256)
        let f = { (d: String) in BenchPack.File(name: "x", url: "", bytes: 0, blake3: "", sha256: d, make: "", model: "",
                                                megapixels: 0, licence: "CC0-1.0") }
        var pack = BenchPack(id: "mini-v1", schemaVersion: 1, licence: "CC0-1.0", minimumPhotos: 2, hash: "",
                             files: digests.map(f))
        pack.hash = pack.expectedHash
        let packURL = dir.appendingPathComponent("mini-v1.json")
        try BenchResults.write(try JSONEncoder().encode(pack), to: packURL)
        var log: [String] = []
        let out = dir.appendingPathComponent("result.json")
        #expect(await BenchCommand.run(.share(results: results, photos: photos, pack: packURL, out: out),
                                       log: { log.append($0) }) == 0, "\(log)")
        let share = try JSONDecoder().decode(BenchShare.self, from: Data(contentsOf: out))
        #expect(share.testPack == .init(id: "mini-v1", hash: pack.hash, ranked: true))
        #expect(share.cameras.first?.files == 2 && share.apps.first?.operations.first?.medianMs == 12)
        #expect(log.last?.contains("ranked") == true)
        let text = try String(contentsOf: out, encoding: .utf8)
        #expect(!text.contains("SKB") && !text.contains(dir.path) && !text.contains(NSUserName()), "no names or paths")

        // Without the pack: custom, not ranked. An empty folder fails.
        #expect(await BenchCommand.run(.share(results: results, photos: photos, pack: nil, out: out), log: { log.append($0) }) == 0)
        #expect(try JSONDecoder().decode(BenchShare.self, from: Data(contentsOf: out)).testPack.id == "custom")
        let empty = dir.appendingPathComponent("empty")
        try FileManager.default.createDirectory(at: empty, withIntermediateDirectories: true)
        #expect(await BenchCommand.run(.share(results: results, photos: empty, pack: nil, out: out), log: { log.append($0) }) == 1)
    }
}
