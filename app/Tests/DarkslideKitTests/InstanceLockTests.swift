import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// One Darkslide GUI per support folder (InstanceLock), checked across processes:
/// `/usr/bin/lockf` takes the same `flock` lock from a second process.
@MainActor @Suite(.serialized) struct InstanceLockTests {
    static func tempRoot() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("instance-lock-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// `lockf -k -t 0 <file> <command…>`: another process that takes the lock (or fails to).
    static func lockf(_ file: URL, _ command: [String]) throws -> Process {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/lockf")
        p.arguments = ["-k", "-s", "-t", "0", file.path] + command
        try p.run()
        return p
    }

    /// Another process holding the lock on `root` (it signals once it holds it).
    static func otherHolder(_ root: URL) throws -> Process {
        let ready = root.appendingPathComponent("ready")
        let other = try lockf(root.appendingPathComponent(InstanceLock.fileName),
                              ["/bin/sh", "-c", "touch '\(ready.path)'; exec /bin/sleep 30"])
        let deadline = Date().addingTimeInterval(10)
        while Date() < deadline, !FileManager.default.fileExists(atPath: ready.path) {
            Thread.sleep(forTimeInterval: 0.02)
        }
        return other
    }

    @Test func aSecondProcessCannotTakeTheLockWhileThisOneHoldsIt() throws {
        let root = try Self.tempRoot()
        defer { try? FileManager.default.removeItem(at: root) }
        guard case .acquired(let lock) = InstanceLock.acquire(in: root) else {
            Issue.record("first acquire failed")
            return
        }
        #expect(InstanceLock.holder(of: lock.url) == .init(host: InstanceLock.hostName, pid: getpid()),
                "host:pid is recorded")
        let other = try Self.lockf(lock.url, ["/usr/bin/true"])
        other.waitUntilExit()
        #expect(other.terminationStatus != 0, "the other process couldn't lock it")
        // A second open in this process conflicts too (flock is per open file).
        guard case .held(let holder) = InstanceLock.acquire(in: root) else {
            Issue.record("second acquire succeeded")
            return
        }
        #expect(holder?.pid == getpid())
        withExtendedLifetime(lock) {}
    }

    @Test func whileAnotherProcessHoldsItTheHolderIsBroughtForwardOrTheUserIsTold() throws {
        let root = try Self.tempRoot()
        defer { try? FileManager.default.removeItem(at: root) }
        let other = try Self.otherHolder(root)
        defer { other.terminate() }
        let file = root.appendingPathComponent(InstanceLock.fileName)
        try Data("\(InstanceLock.hostName):4242\n".utf8).write(to: file) // what a GUI holder records

        // The holder came to the front: a note on stderr, no alert.
        var activated: [InstanceLock.Holder] = []
        var alerts: [String] = []
        var messages: [String] = []
        let started1 = InstanceLock.claimForGUI(support: root, activate: { activated.append($0); return true },
                                          alert: { alerts.append($0) }, report: { messages.append($0) })
        #expect(!started1)
        #expect(activated == [.init(host: InstanceLock.hostName, pid: 4242)])
        #expect(alerts.isEmpty && messages.first?.contains("(pid 4242); switched to it.") == true, "\(messages)")

        // It didn't (still closing, or not a normal app): a visible alert, and no claim it switched.
        messages = []
        let started2 = InstanceLock.claimForGUI(support: root, activate: { _ in false }, alert: { alerts.append($0) },
                                          report: { messages.append($0) })
        #expect(!started2)
        #expect(alerts.count == 1 && alerts[0].contains("may still be closing") && alerts[0].contains("pid 4242"))
        #expect(!(messages.first ?? "").contains("switched"))

        // Nothing recorded (the holder isn't a Darkslide): nothing to activate, the user is told.
        try Data().write(to: file)
        activated = []
        let started3 = InstanceLock.claimForGUI(support: root, activate: { activated.append($0); return true },
                                          alert: { alerts.append($0) }, report: { _ in })
        #expect(!started3)
        #expect(activated.isEmpty && alerts.count == 2 && alerts[1].contains("another process"))

        // Once it quits, a launch takes the lock.
        other.terminate()
        other.waitUntilExit()
        var ok = false
        for _ in 0..<200 {
            if case .acquired = InstanceLock.acquire(in: root) {
                ok = true
                break
            }
            Thread.sleep(forTimeInterval: 0.02)
        }
        #expect(ok)
    }

    @Test func claimForGUIHoldsTheLockForTheProcess() throws {
        let root = try Self.tempRoot()
        defer { try? FileManager.default.removeItem(at: root) }
        var messages: [String] = []
        let started4 = InstanceLock.claimForGUI(support: root, activate: { _ in true }, alert: { _ in },
                                         report: { messages.append($0) })
        #expect(started4)
        #expect(messages.isEmpty)
        let other = try Self.lockf(root.appendingPathComponent(InstanceLock.fileName), ["/usr/bin/true"])
        other.waitUntilExit()
        #expect(other.terminationStatus != 0, "still held after claimForGUI returned")
    }

    @Test func holdersAreOnlyActivatedWhenTheyAreDarkslideOnThisMac() {
        let here = InstanceLock.hostName
        #expect(!here.isEmpty)
        #expect(!InstanceLock.activateHolder(.init(host: "another-mac", pid: getppid())), "another host's pid")
        #expect(!InstanceLock.activateHolder(.init(host: here, pid: getpid())), "never itself")
        #expect(!InstanceLock.activateHolder(.init(host: here, pid: getppid())), "not a Darkslide (the test runner's parent)")
        #expect(!InstanceLock.activateHolder(.init(host: here, pid: 999_999)), "no such process")
    }

    @Test func holderParsingAndAnUnusableSupportFolder() throws {
        let root = try Self.tempRoot()
        defer { try? FileManager.default.removeItem(at: root) }
        let f = root.appendingPathComponent("h")
        for (text, holder) in [("a:12\n", InstanceLock.Holder(host: "a", pid: 12)),
                               ("a:b:7", InstanceLock.Holder(host: "a:b", pid: 7))] {
            try Data(text.utf8).write(to: f)
            #expect(InstanceLock.holder(of: f) == holder)
        }
        for bad in ["", "12", "host:", "host:x"] {
            try Data(bad.utf8).write(to: f)
            #expect(InstanceLock.holder(of: f) == nil, "\(bad)")
        }
        #expect(InstanceLock.holder(of: root.appendingPathComponent("missing")) == nil)

        let notAFolder = root.appendingPathComponent("not-a-folder")
        try Data().write(to: notAFolder)
        guard case .failed(let why) = InstanceLock.acquire(in: notAFolder) else {
            Issue.record("expected a failure")
            return
        }
        #expect(why.contains("can't create"))
        var messages: [String] = []
        let started5 = InstanceLock.claimForGUI(support: notAFolder, activate: { _ in true }, alert: { _ in },
                                         report: { messages.append($0) })
        #expect(started5)
        #expect(messages.first?.contains("starting without it") == true)
    }
}
