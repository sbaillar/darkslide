import AppKit
import LuxtableCore
import LuxtableMac

// `Luxtable --self-test` renders one frame through Rust + Metal and exits.
if CommandLine.arguments.contains("--self-test") {
    Task { @MainActor in
        let checks = await SelfTest.run()
        let failed = checks.filter { !$0.passed }.count
        print(failed == 0 ? "self-test: OK (\(checks.count) checks)" : "self-test: \(failed) FAILED")
        exit(failed == 0 ? 0 : 1)
    }
    dispatchMain()
}

// `Luxtable --bench…`: the benchmark kit (bench/README.md), headless, then exits.
if BenchCommand.wants(CommandLine.arguments) {
    Task { @MainActor in
        do {
            exit(await BenchCommand.run(try BenchCommand.parse(CommandLine.arguments)))
        } catch {
            print("bench: \(error)")
            exit(2)
        }
    }
    dispatchMain()
}

// The first launch after the rename copies Darkslide's preferences and support folder
// (never changing them), before anything here opens or locks the support folder.
MainActor.assumeIsolated { _ = LegacyMigration.runAtLaunch() }

// One GUI per support folder: a second launch on the same library brings the first to the
// front (or says why it can't, in an alert) and exits without opening anything (InstanceLock).
guard MainActor.assumeIsolated({ InstanceLock.claimForGUI() }) else { exit(InstanceLock.alreadyRunningStatus) }

LuxtableApp.main()
