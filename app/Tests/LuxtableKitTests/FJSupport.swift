import CryptoKit
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Final-journeys review harness. Everything lives under $FJ_ROOT (scratch); the
/// app's support root is $LT_SUPPORT_ROOT; preferences are path suites inside FJ_ROOT.
enum FJ {
    static var root: URL {
        URL(fileURLWithPath: ProcessInfo.processInfo.environment["FJ_ROOT"] ?? "/nonexistent-fj-root", isDirectory: true)
    }

    static var logURL: URL { root.appendingPathComponent("fj-log.txt") }

    static func log(_ s: String) {
        let line = "FJ " + s + "\n"
        print(line, terminator: "")
        if let h = try? FileHandle(forWritingTo: logURL) {
            h.seekToEndOfFile()
            h.write(Data(line.utf8))
            try? h.close()
        } else {
            try? Data(line.utf8).write(to: logURL)
        }
    }

    /// The final-journeys scenarios (ported from the review; they build 2.6 GB card
    /// images, so they are opt-in): they run only with `FJ_ROOT` set to a scratch folder
    /// and `LT_SUPPORT_ROOT` set (scripts/test-app.sh sets it), never against the user's
    /// Application Support, Library or Pictures folders.
    ///
    /// The suites carry `.enabled(if: FJ.configured)`, so without it swift-testing
    /// reports them as **skipped** (with the reason), never as passed (review M5).
    static var configured: Bool {
        let env = ProcessInfo.processInfo.environment
        return !(env["FJ_ROOT"] ?? "").isEmpty && !(env["LT_SUPPORT_ROOT"] ?? "").isEmpty
    }

    static func sandboxed() -> Bool {
        let env = ProcessInfo.processInfo.environment
        let home = NSHomeDirectory()
        guard let r = env["FJ_ROOT"], !r.isEmpty, !r.hasPrefix(home + "/Library"), !r.hasPrefix(home + "/Pictures"),
              let s = env["LT_SUPPORT_ROOT"], !s.isEmpty,
              AppPaths.support.standardizedFileURL.path == URL(fileURLWithPath: s).standardizedFileURL.path else {
            print("FJ SKIP: set FJ_ROOT (scratch) and LT_SUPPORT_ROOT to run the journey scenarios")
            return false
        }
        return true
    }

    /// A path-based preferences suite (plist inside FJ_ROOT, never ~/Library/Preferences).
    static func defaults(_ name: String) -> UserDefaults {
        let path = root.appendingPathComponent("prefs-\(name)").path
        return UserDefaults(suiteName: path)!
    }

    /// Unique copies of the samples (a 16-byte trailer keeps BLAKE3s distinct).
    static func uniqueCopies(count: Int, into dir: URL, name: (Int) -> String, clone: Bool = false, salt: String = "") throws -> [URL] {
        let src = sampleARWs()
        precondition(!src.isEmpty)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        var out: [URL] = []
        for i in 0..<count {
            let s = src[i % src.count]
            let d = dir.appendingPathComponent(name(i))
            if clone {
                let p = Process()
                p.executableURL = URL(fileURLWithPath: "/bin/cp")
                p.arguments = ["-c", s.path, d.path]
                try p.run()
                p.waitUntilExit()
            } else {
                try FileManager.default.copyItem(at: s, to: d)
            }
            let h = try FileHandle(forWritingTo: d)
            h.seekToEndOfFile()
            h.write(Data((String(format: "FJUNIQUE%08d", i) + salt).utf8))
            try h.close()
            out.append(d)
        }
        return out
    }

    /// Relative path → "sha mtime size" for every file under root (hidden included).
    static func listing(_ root: URL) async -> [String: String] {
        await cardListing(root)
    }

    static func diff(_ a: [String: String], _ b: [String: String]) -> [String] {
        Set(a.keys).union(b.keys).filter { a[$0] != b[$0] }.sorted().map { k in
            a[k] == nil ? "+\(k)" : (b[k] == nil ? "-\(k)" : "~\(k)")
        }
    }

    static func files(under dir: URL) -> [String] {
        (FileManager.default.enumerator(atPath: dir.path)?.allObjects as? [String] ?? []).filter { rel in
            var d: ObjCBool = false
            return FileManager.default.fileExists(atPath: dir.appendingPathComponent(rel).path, isDirectory: &d) && !d.boolValue
        }.sorted()
    }

    @MainActor
    static func liveModel(prefs: String, sessions: URL, configure: (AppSettings) -> Void = { _ in }) -> AppModel {
        let d = defaults(prefs)
        let settings = AppSettings(defaults: d)
        settings.sessionsFolder = sessions
        configure(settings)
        let m = AppModel.make(mode: .live, settings: settings, defaults: d)
        m.watchCards = false
        m.toastDuration = .seconds(3600) // keep the last toast readable
        return m
    }
}
