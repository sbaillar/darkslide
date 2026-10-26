import AVFoundation
import CoreVideo
import CryptoKit
import Foundation
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// An exFAT "memory card" backed by a disk image (`hdiutil`, no root). Every
/// call has a deadline; on a timeout the test skips (returns nil) and the
/// command is left alone: disk-image processes are never killed.
struct TestCardImage {
    let image: URL
    let mount: URL
    let device: String

    /// Run a command off the caller's actor (tests run on the main actor: a blocked
    /// main thread stalls every other suite); stdout on success, nil on failure or
    /// when it outlives `timeout`.
    static func run(_ args: [String], timeout: TimeInterval = 60) async -> String? {
        await Task.detached { runBlocking(args, timeout: timeout) }.value
    }

    private static func runBlocking(_ args: [String], timeout: TimeInterval) -> String? {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: args[0])
        p.arguments = Array(args.dropFirst())
        let out = Pipe()
        p.standardOutput = out
        p.standardError = Pipe()
        let done = DispatchSemaphore(value: 0)
        p.terminationHandler = { _ in done.signal() }
        do { try p.run() } catch { return nil }
        guard done.wait(timeout: .now() + timeout) == .success else {
            print("SKIP card image: \(args.joined(separator: " ")) timed out")
            return nil
        }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        return p.terminationStatus == 0 ? String(decoding: data, as: UTF8.self) : nil
    }

    /// Create and attach a `sizeMB` exFAT image named `name` under `dir`.
    static func make(in dir: URL, name: String = "IMP6CARD", sizeMB: Int = 256) async -> TestCardImage? {
        let image = dir.appendingPathComponent("\(name).dmg")
        guard await run(["/usr/bin/hdiutil", "create", "-quiet", "-size", "\(sizeMB)m", "-fs", "ExFAT", "-volname", name,
                   image.path]) != nil else {
            print("SKIP card image: hdiutil create failed")
            return nil
        }
        return await attach(image, at: dir.appendingPathComponent("\(name)-mnt"))
    }

    static func attach(_ image: URL, at mount: URL) async -> TestCardImage? {
        try? FileManager.default.createDirectory(at: mount, withIntermediateDirectories: true)
        // -noautoopen: no Finder window on the volume (and none of its metadata writes).
        guard let out = await run(["/usr/bin/hdiutil", "attach", "-nobrowse", "-noautoopen", "-mountpoint", mount.path,
                                   image.path]),
              let dev = out.split(whereSeparator: \.isWhitespace).first else {
            print("SKIP card image: hdiutil attach failed")
            return nil
        }
        return TestCardImage(image: image, mount: mount, device: String(dev))
    }

    /// Detach, then attach again (what the card holds, not what the cache holds).
    func remount() async -> TestCardImage? {
        await detach()
        return await Self.attach(image, at: mount)
    }

    /// Detach and make sure it is gone (L6: a checked detach). Retried, with
    /// `-force` (an hdiutil option) on the last try; no process is ever killed.
    /// `false` when it is still mounted.
    @discardableResult
    func detach() async -> Bool {
        for attempt in 0..<3 {
            // Only the device this image is attached as *now* (from `hdiutil info`): once
            // it is gone, its `/dev/diskN` can be reused by another test's image, and a
            // retry by the remembered number would detach that one instead.
            guard let dev = await Self.attachedDevice(of: image) else { return true }
            _ = await Self.run(["/usr/bin/hdiutil", "detach", "-quiet"] + (attempt == 2 ? ["-force"] : []) + [dev])
            for _ in 0..<20 where isMounted(mount) { try? await Task.sleep(for: .milliseconds(100)) }
            if await Self.attachedDevice(of: image) == nil { return true }
        }
        print("WARNING card image still attached: \(image.path)")
        return false
    }

    /// The whole-disk `/dev/diskN` `image` is attached as now, or nil when it isn't attached.
    static func attachedDevice(of image: URL) async -> String? {
        guard let info = await run(["/usr/bin/hdiutil", "info"]) else { return nil }
        let want = Set([image.path, image.resolvingSymlinksInPath().path])
        for block in info.components(separatedBy: "\n================================================") {
            let lines = block.split(separator: "\n")
            guard let path = lines.first(where: { $0.hasPrefix("image-path") })?
                .split(separator: ":", maxSplits: 1).last?.trimmingCharacters(in: .whitespaces),
                  want.contains(path) || want.contains(URL(fileURLWithPath: path).resolvingSymlinksInPath().path)
            else { continue }
            return lines.first { $0.hasPrefix("/dev/disk") }
                .flatMap { $0.split(whereSeparator: \.isWhitespace).first.map(String.init) }
        }
        return nil
    }
}

/// Whether `url` is the mount point of a mounted volume.
func isMounted(_ url: URL) -> Bool {
    let v = try? url.resourceValues(forKeys: [.volumeURLKey]).volume
    return v?.standardizedFileURL.path == url.standardizedFileURL.path
}

/// Every file under `root`: SHA-256 of its bytes and its modification date
/// (hashed off the caller's actor).
func cardListing(_ root: URL) async -> [String: String] {
    await Task.detached { cardListingBlocking(root) }.value
}

/// Folders macOS itself writes at a mounted volume's root (`.Trashes/._501`,
/// `.fseventsd`…), with nothing of ours involved. They aren't the card's content; every
/// other file (DCIM, PRIVATE, stray files) must stay byte-identical, mtime included.
let osVolumeMetadata: Set<String> = [".Trashes", ".fseventsd", ".Spotlight-V100", ".TemporaryItems"]

private func cardListingBlocking(_ root: URL) -> [String: String] {
    var out: [String: String] = [:]
    let fm = FileManager.default
    guard let e = fm.enumerator(at: root, includingPropertiesForKeys: [.contentModificationDateKey, .isRegularFileKey])
    else { return out }
    for case let url as URL in e {
        if url.deletingLastPathComponent().standardizedFileURL.path == root.standardizedFileURL.path,
           osVolumeMetadata.contains(url.lastPathComponent) {
            e.skipDescendants()
            continue
        }
        let v = try? url.resourceValues(forKeys: [.contentModificationDateKey, .isRegularFileKey])
        guard v?.isRegularFile == true else { continue }
        let rel0 = String(url.standardizedFileURL.path.dropFirst(root.standardizedFileURL.path.count))
        // An unreadable file is a difference, never silently left out of the listing.
        guard let data = try? Data(contentsOf: url) else {
            out[rel0] = "unreadable"
            continue
        }
        let digest = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        let rel = String(url.standardizedFileURL.path.dropFirst(root.standardizedFileURL.path.count))
        out[rel] = "\(digest) \(v?.contentModificationDate?.timeIntervalSince1970 ?? 0)"
    }
    return out
}

/// A tiny, valid H.264 MP4 (16 frames, 64×48) written with AVAssetWriter.
func makeMP4(_ url: URL) async throws {
    try? FileManager.default.removeItem(at: url)
    let writer = try AVAssetWriter(outputURL: url, fileType: .mp4)
    let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
        AVVideoCodecKey: AVVideoCodecType.h264, AVVideoWidthKey: 64, AVVideoHeightKey: 48,
    ])
    let adaptor = AVAssetWriterInputPixelBufferAdaptor(assetWriterInput: input, sourcePixelBufferAttributes: [
        kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
        kCVPixelBufferWidthKey as String: 64, kCVPixelBufferHeightKey as String: 48,
    ])
    writer.add(input)
    writer.startWriting()
    writer.startSession(atSourceTime: .zero)
    for i in 0..<16 {
        while !input.isReadyForMoreMediaData { try await Task.sleep(for: .milliseconds(5)) }
        var pb: CVPixelBuffer?
        CVPixelBufferCreate(nil, 64, 48, kCVPixelFormatType_32BGRA, nil, &pb)
        guard let pb else { continue }
        CVPixelBufferLockBaseAddress(pb, [])
        if let base = CVPixelBufferGetBaseAddress(pb) {
            memset(base, Int32(i * 12), CVPixelBufferGetDataSize(pb))
        }
        CVPixelBufferUnlockBaseAddress(pb, [])
        adaptor.append(pb, withPresentationTime: CMTime(value: CMTimeValue(i), timescale: 8))
    }
    input.markAsFinished()
    await writer.finishWriting()
    if writer.status != .completed { throw writer.error ?? MockError.invalid("mp4") }
}

/// Fill `root` like a Sony card: sample ARWs under DCIM, a clip under PRIVATE,
/// and a file the import leaves behind.
func fillCard(_ root: URL, raws: Int = 3) async throws -> [URL] {
    let fm = FileManager.default
    let dcim = root.appendingPathComponent("DCIM/100MSDCF")
    try fm.createDirectory(at: dcim, withIntermediateDirectories: true)
    var out: [URL] = []
    let sources = Array(sampleARWs().prefix(raws))
    out = try await Task.detached {
        try sources.enumerated().map { i, s in
            let dest = dcim.appendingPathComponent(String(format: "DSC%05d.ARW", 100 + i))
            try FileManager.default.copyItem(at: s, to: dest)
            return dest
        }
    }.value
    let clips = root.appendingPathComponent("PRIVATE/M4ROOT/CLIP")
    try fm.createDirectory(at: clips, withIntermediateDirectories: true)
    let clip = clips.appendingPathComponent("C0001.MP4")
    try await makeMP4(clip)
    out.append(clip)
    try Data("camera junk".utf8).write(to: root.appendingPathComponent("DCIM/notes.txt"))
    return out
}

/// A verifying import service that returns a fixed report (eject-gating tests).
struct ReportingImportService: ImportVerifying {
    var scan: ImportScan
    var make: @Sendable (ImportPlan) -> ImportReport

    func scan(volume: URL) async throws -> [ImportCandidate] { scan.candidates }
    func scanCard(volume: URL) async throws -> ImportScan { scan }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> {
        AsyncThrowingStream { c in
            c.yield(ImportProgress(filesDone: plan.candidates.count, filesTotal: plan.candidates.count, bytesDone: 10,
                                   bytesTotal: 10))
            c.finish()
        }
    }

    func report(for plan: ImportPlan) async -> ImportReport? { make(plan) }
}

/// A session store holding one photo and one clip (video exclusion tests).
struct VideoStore: SessionStore {
    static let session = SessionInfo(name: "Clips", url: URL(fileURLWithPath: "/tmp/clips"), photoCount: 2, createdAt: Date())

    func listSessions() async throws -> [SessionInfo] { [Self.session] }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        var photo = samplePhoto(1)
        photo.url = URL(fileURLWithPath: "/tmp/clips/DSC00001.ARW")
        var clip = samplePhoto(2)
        clip.filename = "C0001.MP4"
        clip.url = URL(fileURLWithPath: "/tmp/clips/C0001.MP4")
        return [photo, clip]
    }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { Self.session }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
}
