import Foundation

/// Where Luxtable keeps its own state (global index, fallback stores, presets,
/// Amazon manifest, archive locks).
///
/// Normally `~/Library/Application Support/Luxtable`. `$LT_SUPPORT_ROOT`
/// overrides it, and `--snapshot` runs use a scratch folder so dev and
/// screenshot runs never write the real one (like their own defaults suite).
public enum AppPaths {
    public static var support: URL {
        support(environment: ProcessInfo.processInfo.environment, arguments: CommandLine.arguments)
    }

    /// Installed plug-ins and their settings (`state.json`).
    public static var plugins: URL { support.appendingPathComponent("Plugins", isDirectory: true) }

    /// The bundled plug-ins: the app's `Resources/Plugins`; outside an app bundle (the
    /// tests, `swift run`) the source tree's `plugins/builtin`. Either way they load only
    /// when their files match the hashes compiled into this build.
    public static var builtInPlugins: URL? {
        if let bundled = Bundle.main.resourceURL?.appendingPathComponent("Plugins", isDirectory: true),
           FileManager.default.fileExists(atPath: bundled.path) {
            return bundled
        }
        // A released app never falls back to a path on the machine that built it (and a
        // release build doesn't carry that path: scripts/build-app.sh refuses $HOME in the binary).
        #if DEBUG
        guard Bundle.main.bundleURL.pathExtension != "app" else { return nil }
        let source = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("plugins/builtin", isDirectory: true)
        return FileManager.default.fileExists(atPath: source.path) ? source : nil
        #else
        return nil
        #endif
    }

    static func support(environment: [String: String], arguments: [String]) -> URL {
        if let env = LaunchEnvironment.value("LT_SUPPORT_ROOT", in: environment), !env.isEmpty {
            return URL(fileURLWithPath: env, isDirectory: true)
        }
        if arguments.contains("--snapshot") {
            return FileManager.default.temporaryDirectory.appendingPathComponent("luxtable-snapshot-support", isDirectory: true)
        }
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Library/Application Support")
        return base.appendingPathComponent("Luxtable", isDirectory: true)
    }
}
