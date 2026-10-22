import AppKit
import CoreMedia
import ScreenCaptureKit

/// The real screen for `BenchDriver`: ScreenCaptureKit records the viewer region at up
/// to 120 fps (scaled down to a small luma grid on the GPU), CGEvent posts keys and
/// clicks, NSAppleScript runs script stimuli. Frame times and `now()` are both on the
/// host clock, so a latency is a frame's presentation time minus the stimulus time.
///
/// Events go out only while the app under test is frontmost (`BenchDriver` checks
/// `frontmostPID` before each one).
///
/// Needs, for the process that runs it (the terminal): Screen Recording, Accessibility
/// (posting events) and Automation (scripts). Real hardware only: no test runs this.
@MainActor
public final class SystemBenchScreen: BenchScreen {
    public enum ScreenError: Error, CustomStringConvertible {
        case notRunning(String)
        case noWindow(String)
        case noDisplay
        case script(String)
        case several(String, Int)

        public var description: String {
            switch self {
            case .several(let p, let n): "\(n) instances of \(p) are running: quit all but the bench one (or pass --pid)"
            case .notRunning(let p): "\(p) is not running"
            case .noWindow(let p): "\(p) has no window on screen"
            case .noDisplay: "no display to record"
            case .script(let e): "AppleScript stimulus failed: \(e)"
            }
        }
    }

    /// Width of the recorded luma grid (height follows the region's aspect).
    let gridWidth = 192
    private var stream: SCStream?
    private var sink: LumaSink?
    private var scripts: [String: NSAppleScript] = [:]
    /// Drive this process only (an app the script launched), whatever its name.
    let pid: pid_t?

    public init(pid: pid_t? = nil) {
        self.pid = pid
    }

    public func activate(process: String) async throws -> (pid: pid_t, window: CGRect) {
        // Never this process (the driver is a Darkslide binary too).
        let matches = NSWorkspace.shared.runningApplications.filter {
            guard $0.processIdentifier != getpid() else { return false }
            if let pid { return $0.processIdentifier == pid }
            return $0.localizedName == process || ($0.localizedName?.hasPrefix(process) ?? false)
        }
        guard let app = matches.first else { throw ScreenError.notRunning(pid.map { "\(process) (pid \($0))" } ?? process) }
        // Exactly one: with two (an old and a new version, or the user's own copy) the
        // events could reach the wrong one.
        guard matches.count == 1 else { throw ScreenError.several(process, matches.count) }
        app.activate()
        try? await Task.sleep(for: .milliseconds(800))
        let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
            as? [[String: Any]] ?? []
        let frames = windows.compactMap { w -> CGRect? in
            guard (w[kCGWindowOwnerPID as String] as? pid_t) == app.processIdentifier,
                  (w[kCGWindowLayer as String] as? Int) == 0,
                  let b = w[kCGWindowBounds as String] as? NSDictionary else { return nil }
            return CGRect(dictionaryRepresentation: b as CFDictionary)
        }
        guard let main = frames.max(by: { $0.width * $0.height < $1.width * $1.height }) else {
            throw ScreenError.noWindow(process)
        }
        return (app.processIdentifier, main)
    }

    public func frontmostPID() -> pid_t? { NSWorkspace.shared.frontmostApplication?.processIdentifier }

    public func prepare(_ stimuli: [BenchPlan.Stimulus]) throws {
        for case .script(let text) in stimuli where scripts[text] == nil {
            scripts[text] = try compiled(text)
        }
    }

    public func evaluate(_ script: String) throws -> String {
        let s = try scripts[script] ?? compiled(script)
        scripts[script] = s
        var error: NSDictionary?
        let result = s.executeAndReturnError(&error)
        if let error { throw ScreenError.script("\(error)") }
        return result.stringValue ?? ""
    }

    func compiled(_ text: String) throws -> NSAppleScript {
        guard let s = NSAppleScript(source: text) else { throw ScreenError.script("can't create \(text)") }
        var error: NSDictionary?
        guard s.compileAndReturnError(&error) else { throw ScreenError.script("doesn't compile: \(error ?? [:])") }
        return s
    }

    public func startRecording(_ region: CGRect) async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        guard let display = content.displays.first(where: { CGDisplayBounds($0.displayID).contains(region.origin) })
            ?? content.displays.first else { throw ScreenError.noDisplay }
        let bounds = CGDisplayBounds(display.displayID)
        let config = SCStreamConfiguration()
        config.sourceRect = region.offsetBy(dx: -bounds.minX, dy: -bounds.minY)
        config.width = gridWidth
        config.height = max(1, Int((Double(gridWidth) * region.height / max(1, region.width)).rounded()))
        config.minimumFrameInterval = CMTime(value: 1, timescale: 120)
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.showsCursor = false
        config.queueDepth = 6
        let sink = LumaSink()
        let stream = SCStream(filter: SCContentFilter(display: display, excludingWindows: []), configuration: config,
                              delegate: nil)
        try stream.addStreamOutput(sink, type: .screen, sampleHandlerQueue: DispatchQueue(label: "bench.capture"))
        try await stream.startCapture()
        self.sink = sink
        self.stream = stream
    }

    public func stopRecording() async throws -> [LumaFrame] {
        try await stream?.stopCapture()
        defer {
            stream = nil
            sink = nil
        }
        return sink?.frames ?? []
    }

    public func send(_ stimulus: BenchPlan.Stimulus, window: CGRect) throws {
        let source = CGEventSource(stateID: .hidSystemState)
        switch stimulus {
        case .key(let code, let modifiers):
            var flags: CGEventFlags = []
            for m in modifiers {
                switch m {
                case "cmd": flags.insert(.maskCommand)
                case "shift": flags.insert(.maskShift)
                case "opt": flags.insert(.maskAlternate)
                case "ctrl": flags.insert(.maskControl)
                default: break
                }
            }
            for down in [true, false] {
                let e = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down)
                e?.flags = flags
                e?.post(tap: .cghidEventTap)
            }
        case .click(let x, let y):
            let p = CGPoint(x: window.minX + x * window.width, y: window.minY + y * window.height)
            for type in [CGEventType.leftMouseDown, .leftMouseUp] {
                CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: p, mouseButton: .left)?
                    .post(tap: .cghidEventTap)
            }
        case .script(let text):
            let script = try scripts[text] ?? compiled(text) // compiled in `prepare`
            scripts[text] = script
            var error: NSDictionary?
            script.executeAndReturnError(&error)
            if let error { throw ScreenError.script("\(error)") }
        }
    }

    public func now() -> Double { CMClockGetTime(CMClockGetHostTimeClock()).seconds }

    public func wait(_ seconds: Double) async {
        try? await Task.sleep(for: .milliseconds(Int(seconds * 1000)))
    }
}

/// Collects complete frames as luma grids (called on the capture queue).
final class LumaSink: NSObject, SCStreamOutput, @unchecked Sendable {
    private let lock = NSLock()
    private var collected: [LumaFrame] = []

    var frames: [LumaFrame] { lock.withLock { collected } }

    func stream(_ stream: SCStream, didOutputSampleBuffer buffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen,
              let info = CMSampleBufferGetSampleAttachmentsArray(buffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let raw = info.first?[.status] as? Int, SCFrameStatus(rawValue: raw) == .complete,
              let pixels = buffer.imageBuffer else { return }
        CVPixelBufferLockBaseAddress(pixels, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pixels, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pixels) else { return }
        let w = CVPixelBufferGetWidth(pixels), h = CVPixelBufferGetHeight(pixels)
        let rowBytes = CVPixelBufferGetBytesPerRow(pixels)
        let bgra = UnsafeRawBufferPointer(start: base, count: rowBytes * h)
        let luma = Self.luma(bgra: bgra, width: w, height: h, rowBytes: rowBytes)
        let frame = LumaFrame(t: CMSampleBufferGetPresentationTimeStamp(buffer).seconds, width: w, height: h, pixels: luma)
        lock.withLock { collected.append(frame) }
    }

    /// Rec. 709 luma of BGRA rows.
    static func luma(bgra: UnsafeRawBufferPointer, width: Int, height: Int, rowBytes: Int) -> [UInt8] {
        var out = [UInt8](repeating: 0, count: width * height)
        for y in 0..<height {
            for x in 0..<width {
                let i = y * rowBytes + x * 4
                let v = (19 * Int(bgra[i]) + 183 * Int(bgra[i + 1]) + 54 * Int(bgra[i + 2])) >> 8
                out[y * width + x] = UInt8(v)
            }
        }
        return out
    }
}
