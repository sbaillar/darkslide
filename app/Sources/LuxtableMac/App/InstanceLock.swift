import AppKit
import LuxtableCore
import LuxtableUI
import Foundation

/// One Luxtable per support folder: the GUI holds an exclusive `flock` on
/// `<support>/instance.lock` for as long as it runs. A second process on the same
/// support folder (the same library and preferences) activates the first one and exits
/// without opening anything, so two instances never write the same sessions, caches
/// and preferences. The `--bench` and `--self-test` modes never take it (they run on
/// their own scratch `LT_SUPPORT_ROOT`).
public final class InstanceLock: @unchecked Sendable {
    public static let fileName = "instance.lock"
    /// Exit status of a launch that found Luxtable already running.
    public static let alreadyRunningStatus: Int32 = 3

    public let url: URL
    private let fd: Int32

    private init(url: URL, fd: Int32) {
        self.url = url
        self.fd = fd
    }

    deinit { close(fd) } // closing the descriptor releases the lock

    /// Who holds a lock: the host and pid it recorded.
    public struct Holder: Equatable, Sendable {
        public var host: String
        public var pid: pid_t
    }

    public enum Outcome {
        case acquired(InstanceLock)
        /// Another process holds it (who, when it recorded itself).
        case held(by: Holder?)
        case failed(String)
    }

    /// This Mac's host name (`gethostname`, no network lookup).
    static var hostName: String {
        var buf = [CChar](repeating: 0, count: 256)
        guard gethostname(&buf, buf.count) == 0 else { return "" }
        return String(decoding: buf.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
    }

    /// Take the lock in `support` without waiting, and record `host:pid` in it.
    public static func acquire(in support: URL) -> Outcome {
        do {
            try FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
        } catch {
            return .failed("can't create \(support.path): \(error.localizedDescription)")
        }
        let url = support.appendingPathComponent(fileName)
        let fd = open(url.path, O_RDWR | O_CREAT | O_CLOEXEC, 0o644)
        guard fd >= 0 else { return .failed("can't open \(url.path): \(String(cString: strerror(errno)))") }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else {
            let busy = errno == EWOULDBLOCK
            let why = String(cString: strerror(errno))
            close(fd)
            return busy ? .held(by: holder(of: url)) : .failed("can't lock \(url.path): \(why)")
        }
        let me = Data("\(hostName):\(getpid())\n".utf8)
        _ = ftruncate(fd, 0)
        _ = me.withUnsafeBytes { pwrite(fd, $0.baseAddress, me.count, 0) }
        return .acquired(InstanceLock(url: url, fd: fd))
    }

    /// The `host:pid` recorded in a lock file (nil when empty, unreadable or malformed).
    static func holder(of url: URL) -> Holder? {
        guard let text = try? String(contentsOf: url, encoding: .utf8) else { return nil }
        let line = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let colon = line.lastIndex(of: ":"), let pid = pid_t(line[line.index(after: colon)...]) else { return nil }
        return Holder(host: String(line[..<colon]), pid: pid)
    }

    /// Held for the life of the GUI process.
    nonisolated(unsafe) private static var held: InstanceLock?

    /// Bring the holder to the front: only a Luxtable (same bundle id) on this Mac, as a
    /// regular app; true when it really became active.
    @MainActor
    public static func activateHolder(_ holder: Holder) -> Bool {
        guard holder.host == hostName, holder.pid != getpid(),
              let app = NSRunningApplication(processIdentifier: holder.pid),
              let me = Bundle.main.bundleIdentifier, app.bundleIdentifier == me, app.activationPolicy == .regular,
              app.activate() else { return false }
        RunLoop.current.run(until: Date().addingTimeInterval(0.5))
        return app.isActive
    }

    /// A visible notice for a launch that can't go on (a Finder or Dock launch has no stderr).
    /// `LT_NO_ALERTS=1` (unattended scripts) leaves it on stderr only.
    @MainActor
    public static func showAlreadyRunning(_ message: String) {
        guard LaunchEnvironment.value("LT_NO_ALERTS") != "1" else { return }
        NSApplication.shared.setActivationPolicy(.regular)
        NSApp.activate()
        let alert = NSAlert()
        alert.messageText = "Luxtable is already running or still closing"
        alert.informativeText = message
        alert.addButton(withTitle: "Quit")
        alert.runModal()
    }

    /// The GUI launch check (main.swift, before the app starts). True: this process holds
    /// the lock and may start. False: another Luxtable on `support` holds it: it was
    /// brought to the front, or, when that didn't happen (it is still closing, or isn't a
    /// normal app), the user was told in an alert; the caller exits.
    @MainActor
    public static func claimForGUI(support: URL = AppPaths.support,
                                   activate: @MainActor (Holder) -> Bool = activateHolder,
                                   alert: @MainActor (String) -> Void = showAlreadyRunning,
                                   report: (String) -> Void = { FileHandle.standardError.write(Data(($0 + "\n").utf8)) })
        -> Bool {
        switch acquire(in: support) {
        case .acquired(let lock):
            held = lock
            return true
        case .held(let holder):
            let who = holder.map { "pid \($0.pid)" + ($0.host == hostName ? "" : " on \($0.host)") } ?? "another process"
            if let holder, activate(holder) {
                report("Luxtable is already running with \(support.path) (\(who)); switched to it.")
            } else {
                let message = "Another Luxtable (\(who)) is using \(support.path) and didn't come to the front. "
                    + "It may still be closing, for example while waiting on a disconnected network volume. "
                    + "Wait a moment, or quit it in Activity Monitor, then open Luxtable again."
                report("Luxtable: " + message)
                alert(message)
            }
            return false
        case .failed(let why):
            // Can't tell: start anyway rather than refuse to open at all.
            report("Luxtable: instance lock unavailable (\(why)); starting without it.")
            return true
        }
    }
}
