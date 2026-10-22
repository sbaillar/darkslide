import Foundation

/// Fake card import: lists a synthetic DCIM folder and "copies" at card speed.
/// Stands in for `ds_import` until the import facade is exported.
public struct MockImportService: ImportService {
    public var bytesPerSecond: Double
    public var alreadyImportedEvery: Int
    /// Files invented when the volume has no real DCIM contents.
    public var inventedCount: Int
    /// Simulate a pulled card: fail after this many files (nil = never).
    public var failAfter: Int?

    public init(bytesPerSecond: Double = 280_000_000, alreadyImportedEvery: Int = 7, inventedCount: Int = 120,
                failAfter: Int? = nil) {
        self.bytesPerSecond = bytesPerSecond
        self.alreadyImportedEvery = alreadyImportedEvery
        self.inventedCount = inventedCount
        self.failAfter = failAfter
    }

    public static let supportedExtensions: Set<String> = ["arw", "jpg", "jpeg", "cr3", "cr2", "nef", "dng", "raf", "heic"]

    public func scan(volume: URL) async throws -> [ImportCandidate] {
        let dcim = volume.appendingPathComponent("DCIM")
        let fm = FileManager.default
        // A real card: list what's there. Otherwise invent a plausible roll.
        if let files = try? fm.subpathsOfDirectory(atPath: dcim.path), !files.isEmpty {
            return files.sorted().compactMap { rel -> ImportCandidate? in
                let url = dcim.appendingPathComponent(rel)
                guard Self.supportedExtensions.contains(url.pathExtension.lowercased()),
                      let attrs = try? fm.attributesOfItem(atPath: url.path) else { return nil }
                return ImportCandidate(url: url, size: (attrs[.size] as? NSNumber)?.int64Value ?? 0,
                                       captureDate: (attrs[.modificationDate] as? Date) ?? Date(), alreadyImported: false)
            }
        }
        let start = Date().addingTimeInterval(-3600)
        return (0..<inventedCount).map { (i: Int) -> ImportCandidate in
            let name = String(format: "100MSDCF/DSC%05d.ARW", 4200 + i)
            let dup = alreadyImportedEvery > 0 && i < 20 && i % alreadyImportedEvery == 0
            return ImportCandidate(url: dcim.appendingPathComponent(name), size: 48_500_000,
                                   captureDate: start.addingTimeInterval(Double(i) * 2.5), alreadyImported: dup)
        }
    }

    public func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> {
        let files = plan.candidates
        let bps = bytesPerSecond
        let failAfter = failAfter
        return AsyncThrowingStream { cont in
            let task = Task {
                let total = files.reduce(Int64(0)) { $0 + $1.size }
                var done: Int64 = 0
                cont.yield(ImportProgress(filesDone: 0, filesTotal: files.count, bytesDone: 0, bytesTotal: total))
                for (i, f) in files.enumerated() {
                    if let failAfter, i >= failAfter {
                        cont.finish(throwing: MockError.invalid("The card was removed during import"))
                        return
                    }
                    do {
                        try await Task.sleep(for: .seconds(Double(f.size) / bps))
                    } catch {
                        cont.finish(throwing: error)
                        return
                    }
                    done += f.size
                    cont.yield(ImportProgress(filesDone: i + 1, filesTotal: files.count, bytesDone: done,
                                              bytesTotal: total, currentFile: f.filename))
                }
                cont.finish()
            }
            cont.onTermination = { _ in task.cancel() }
        }
    }
}
