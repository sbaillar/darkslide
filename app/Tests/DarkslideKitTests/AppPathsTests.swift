import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Dev and screenshot runs never write the real Application Support folder
/// (a snapshot run had put its imports into the real global index).
@Suite struct AppPathsTests {
    @Test func supportFolderHonoursOverrideAndSnapshotRuns() {
        let real = AppPaths.support(environment: [:], arguments: ["Darkslide"])
        #expect(real.path.hasSuffix("Library/Application Support/Darkslide"))
        #expect(AppPaths.support(environment: ["DS_SUPPORT_ROOT": "/tmp/x"], arguments: ["--snapshot"]).path == "/tmp/x")
        let snap = AppPaths.support(environment: [:], arguments: ["Darkslide", "--snapshot", "o.png"])
        #expect(snap != real && snap.path.hasPrefix(FileManager.default.temporaryDirectory.path))
        #expect(AppPaths.support(environment: ["DS_SUPPORT_ROOT": ""], arguments: []) == real, "empty override ignored")
    }

    /// M4: which preferences domain a process uses. Only a plain run uses the user's.
    @MainActor @Test func defaultsSuiteIsPrivateForSnapshotDevAndExplicitRuns() {
        #expect(AppModel.defaultsSuiteName(arguments: ["Darkslide"], environment: [:]) == nil)
        #expect(AppModel.defaultsSuiteName(arguments: ["Darkslide", "--snapshot", "o.png"], environment: [:]) == AppModel.snapshotSuite)
        #expect(AppModel.defaultsSuiteName(arguments: ["D", "--defaults-suite", "x.y"], environment: ["DS_SUPPORT_ROOT": "/r"]) == "x.y")
        let a = AppModel.defaultsSuiteName(arguments: ["D", "--snapshot", "o.png"], environment: ["DS_SUPPORT_ROOT": "/a"])
        let b = AppModel.defaultsSuiteName(arguments: ["D"], environment: ["DS_SUPPORT_ROOT": "/b"])
        #expect(a == "/a/preferences" && b == "/b/preferences", "a plist inside each support root")
        #expect(AppModel.defaults(arguments: ["D"], environment: ["DS_SUPPORT_ROOT": "/a"]) !== UserDefaults.standard)
    }

    /// Under scripts/test-app.sh (DS_SUPPORT_ROOT set) nothing in the app writes the user's domain.
    @MainActor @Test func testProcessesUsePrivateDefaults() {
        guard let root = ProcessInfo.processInfo.environment["DS_SUPPORT_ROOT"], !root.isEmpty else { return }
        #expect(AppModel.usesPrivateDefaults)
        #expect(AppModel.appDefaults !== UserDefaults.standard)
        #expect(ToolLayout.shared.defaultsForTesting !== UserDefaults.standard)
    }

    @MainActor @Test func everyStateFileLivesUnderTheSupportFolder() {
        let base = AppPaths.support.path
        #expect(RustImportService.defaultGlobalIndex.path.hasPrefix(base))
        #expect(RustArchiveService.defaultManifest.path.hasPrefix(base))
        #expect(RustArchiveService.defaultLockDir.hasPrefix(base))
        #expect(RustSessionStore.defaultStoreBase.path.hasPrefix(base))
        #expect(PresetStore.defaultURL?.path.hasPrefix(base) == true)
    }
}
