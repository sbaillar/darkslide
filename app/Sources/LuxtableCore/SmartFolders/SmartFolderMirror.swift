import Foundation

/// What `lstat` says about a path (symlinks are never followed).
public enum MirrorEntryKind: Sendable, Equatable {
    case missing, directory, symlink, other
}

/// The file operations "Mirror to Finder" needs (a fake in unit tests).
/// Nothing here follows a symlink in the last path component.
public protocol MirrorFileSystem: Sendable {
    func kind(_ url: URL) -> MirrorEntryKind
    /// Create one directory level (`mkdir(2)`); an existing *directory* is fine.
    func makeDirectory(_ url: URL) throws
    /// Entries of `dir` (not recursive); empty when it doesn't exist.
    func contents(of dir: URL) throws -> [URL]
    func symlinkDestination(_ url: URL) throws -> String
    func createSymlink(at url: URL, to destination: URL) throws
    /// Removes the link at `url` (the mirror only ever passes its own symlinks).
    func removeLink(_ url: URL) throws
    /// Real path of an existing directory (symlinks resolved), else `url` standardized.
    func canonicalDirectory(_ url: URL) -> URL
    /// Contents of a regular file (`nil` when missing or not a regular file).
    func read(_ url: URL) -> Data?
    /// Atomic write (temp + rename: replaces a symlink at `url`, never writes through it).
    func write(_ data: Data, to url: URL) throws
}

extension MirrorFileSystem {
    public func isSymlink(_ url: URL) -> Bool { kind(url) == .symlink }
}

public struct RealMirrorFileSystem: MirrorFileSystem {
    public init() {}

    public func kind(_ url: URL) -> MirrorEntryKind {
        var st = stat()
        guard lstat(url.path, &st) == 0 else { return .missing }
        switch st.st_mode & S_IFMT {
        case S_IFDIR: return .directory
        case S_IFLNK: return .symlink
        default: return .other
        }
    }

    public func makeDirectory(_ url: URL) throws {
        if mkdir(url.path, 0o755) == 0 { return }
        let err = errno
        guard err == EEXIST, kind(url) == .directory else { throw MirrorError.io(url.path, err) }
    }

    public func contents(of dir: URL) throws -> [URL] {
        guard kind(dir) == .directory else { return [] }
        return try FileManager.default.contentsOfDirectory(atPath: dir.path).map { dir.appendingPathComponent($0) }
    }

    public func symlinkDestination(_ url: URL) throws -> String {
        try FileManager.default.destinationOfSymbolicLink(atPath: url.path)
    }

    public func createSymlink(at url: URL, to destination: URL) throws {
        // symlink(2) fails with EEXIST on any existing entry: never replaces, never follows.
        guard symlink(destination.path, url.path) == 0 else { throw MirrorError.io(url.path, errno) }
    }

    public func removeLink(_ url: URL) throws {
        // unlink(2) removes the link itself and fails on directories: never follows.
        guard unlink(url.path) == 0 else { throw MirrorError.io(url.path, errno) }
    }

    public func canonicalDirectory(_ url: URL) -> URL {
        guard let p = realpath(url.path, nil) else { return url.standardizedFileURL }
        defer { free(p) }
        return URL(fileURLWithPath: String(cString: p), isDirectory: true)
    }

    public func read(_ url: URL) -> Data? {
        guard kind(url) == .other else { return nil }
        let fd = open(url.path, O_RDONLY | O_NOFOLLOW)
        guard fd >= 0 else { return nil }
        return FileHandle(fileDescriptor: fd, closeOnDealloc: true).readDataToEndOfFile()
    }

    public func write(_ data: Data, to url: URL) throws {
        try data.write(to: url, options: .atomic)
    }
}

public enum MirrorError: Error, Equatable {
    /// Refused: a symlinked mirror folder/root, or deleting something that isn't the mirror's own link.
    case refused(String)
    case io(String, Int32)
}

/// One mirrored smart folder.
public struct MirrorFolder: Sendable {
    public let id: Int64
    public let name: String
    public let members: [Photo]

    public init(id: Int64, name: String, members: [Photo]) {
        self.id = id
        self.name = name
        self.members = members
    }
}

/// What one `sync` did. A folder that failed is listed in `errors`; the others still ran.
public struct MirrorReport: Equatable, Sendable {
    public var created = 0
    public var removed = 0
    public var errors: [String] = []
}

/// Links the mirror created, per smart folder id (`<root>/.luxtable-mirror.json`).
/// Only these are ever removed, and only while they still point where the mirror put them.
struct MirrorManifest: Codable, Equatable {
    struct Entry: Codable, Equatable {
        var dir: String
        /// Link name → target path.
        var links: [String: String]
    }

    var folders: [String: Entry] = [:]
}

/// Keeps `<root>/<folder name>/` as a folder of symlinks to the members'
/// originals. It only ever creates symlinks, and only deletes symlinks it
/// created itself (recorded in a manifest): never copies, never hard links,
/// never touches an original or the user's own files and links. A symlinked
/// root or mirror folder is refused (nothing is written through it).
public struct SmartFolderMirror: Sendable {
    public let root: URL
    let fs: any MirrorFileSystem
    static let manifestName = ".luxtable-mirror.json"
    /// Migration source: the manifest's name before the rename. Read when there is no
    /// new one (the links it lists are still ours); the next save writes the new name.
    static let legacyManifestName = ".darkslide-mirror.json"

    public init(root: URL, fs: any MirrorFileSystem = RealMirrorFileSystem()) {
        self.root = root.standardizedFileURL
        self.fs = fs
    }

    /// `root` with its parent's symlinks resolved (so later parent checks compare real paths).
    public var canonicalRoot: URL {
        fs.canonicalDirectory(root.deletingLastPathComponent())
            .appendingPathComponent(root.lastPathComponent, isDirectory: true)
    }

    /// Finder-safe folder name.
    public static func directoryName(_ name: String) -> String {
        var n = name.replacingOccurrences(of: "/", with: "-").replacingOccurrences(of: ":", with: "-")
        n = n.trimmingCharacters(in: .whitespacesAndNewlines)
        while n.hasPrefix(".") { n.removeFirst() }
        return n.isEmpty ? "Untitled" : n
    }

    /// A unique folder name per smart folder, case- and normalization-insensitive
    /// (APFS/HFS+ default): collisions get a short id suffix.
    public static func directoryNames(_ folders: [(id: Int64, name: String)]) -> [Int64: String] {
        var used: Set<String> = []
        var out: [Int64: String] = [:]
        for f in folders {
            let base = directoryName(f.name)
            var name = base
            var n = 0
            while used.contains(key(name)) {
                let short = String(UInt64(bitPattern: f.id) & 0xFFFF, radix: 16)
                name = n == 0 ? "\(base) (\(short))" : "\(base) (\(short)-\(n))"
                n += 1
            }
            used.insert(key(name))
            out[f.id] = name
        }
        return out
    }

    /// File-system name comparison key (case- and normalization-insensitive).
    static func key(_ name: String) -> String { name.precomposedStringWithCanonicalMapping.lowercased() }

    public func directory(for name: String) -> URL {
        canonicalRoot.appendingPathComponent(Self.directoryName(name), isDirectory: true)
    }

    // MARK: - Sync

    /// Make each folder's mirror hold exactly one link per member with an
    /// original. Links of folders not in `folders` (turned off, deleted,
    /// renamed) are removed when `clearOthers`. Throws only when the root is unusable.
    @discardableResult
    public func sync(_ folders: [MirrorFolder], clearOthers: Bool = true) throws -> MirrorReport {
        let root = try prepareRoot()
        var manifest = loadManifest(root)
        var report = MirrorReport()
        let names = Self.directoryNames(folders.map { ($0.id, $0.name) })
        let wanted = Set(folders.map { String($0.id) })
        for (id, entry) in manifest.folders where clearOthers && !wanted.contains(id) {
            report.removed += removeOwned(entry, root: root)
            manifest.folders[id] = nil
        }
        for f in folders {
            let key = String(f.id)
            do {
                let (c, r) = try syncOne(f, dirName: names[f.id]!, root: root, owned: &manifest.folders[key])
                report.created += c
                report.removed += r
            } catch {
                report.errors.append("\(f.name): \(Self.describe(error))")
            }
        }
        try saveManifest(manifest, root: root)
        return report
    }

    /// One folder (convenience; throws on its error).
    @discardableResult
    public func sync(name: String, id: Int64 = 0, members: [Photo]) throws -> (created: Int, removed: Int) {
        let r = try sync([MirrorFolder(id: id, name: name, members: members)], clearOthers: false)
        if let e = r.errors.first { throw MirrorError.refused(e) }
        return (r.created, r.removed)
    }

    /// Remove every link the mirror made for folder `id` (mirroring turned off,
    /// folder deleted). The folder itself and anything else in it stay.
    @discardableResult
    public func clear(id: Int64) throws -> Int {
        let root = canonicalRoot
        guard fs.kind(root) == .directory else { return 0 }
        var manifest = loadManifest(root)
        guard let entry = manifest.folders[String(id)] else { return 0 }
        let n = removeOwned(entry, root: root)
        manifest.folders[String(id)] = nil
        try saveManifest(manifest, root: root)
        return n
    }

    private func prepareRoot() throws -> URL {
        let root = canonicalRoot
        switch fs.kind(root) {
        case .missing: try fs.makeDirectory(root)
        case .directory: break
        case .symlink, .other: throw MirrorError.refused(root.path)
        }
        guard fs.kind(root) == .directory else { throw MirrorError.refused(root.path) }
        return root
    }

    private func syncOne(_ f: MirrorFolder, dirName: String, root: URL,
                         owned: inout MirrorManifest.Entry?) throws -> (Int, Int) {
        var removed = 0
        // Renamed: the old folder's links go first.
        if let o = owned, o.dir != dirName {
            removed += removeOwned(o, root: root)
            owned = nil
        }
        let dir = root.appendingPathComponent(dirName, isDirectory: true)
        switch fs.kind(dir) {
        case .missing: try fs.makeDirectory(dir)
        case .directory: break
        case .symlink, .other: throw MirrorError.refused(dir.path)
        }
        guard fs.kind(dir) == .directory else { throw MirrorError.refused(dir.path) }

        // Wanted links, unique by file-system key.
        var want: [String: (name: String, target: URL)] = [:]
        for p in f.members {
            guard let url = p.url else { continue }
            var link = url.lastPathComponent
            if want[Self.key(link)] != nil {
                let ext = url.pathExtension
                link = "\(url.deletingPathExtension().lastPathComponent) (\(p.id.raw))" + (ext.isEmpty ? "" : ".\(ext)")
            }
            guard want[Self.key(link)] == nil else { continue }
            want[Self.key(link)] = (link, url)
        }

        var links = owned?.links ?? [:]
        for (name, target) in links {
            let url = dir.appendingPathComponent(name)
            // Still ours only while it's the symlink we made, pointing where we pointed it.
            guard fs.kind(url) == .symlink, (try? fs.symlinkDestination(url)) == target else {
                links[name] = nil
                continue
            }
            if let w = want[Self.key(name)], w.name == name, w.target.path == target {
                want[Self.key(name)] = nil
            } else if (try? removeLink(url, in: dir)) != nil {
                links[name] = nil
                removed += 1
            }
        }
        var created = 0
        let existing = Set(try fs.contents(of: dir).map { Self.key($0.lastPathComponent) })
        // Anything already there with a link's name (the user's own file or link) stays; that member is skipped.
        for (k, w) in want.sorted(by: { $0.key < $1.key }) where !existing.contains(k) {
            do {
                try fs.createSymlink(at: dir.appendingPathComponent(w.name), to: w.target)
                links[w.name] = w.target.path
                created += 1
            } catch {
                continue // raced with a new entry of that name: skip this member
            }
        }
        owned = MirrorManifest.Entry(dir: dirName, links: links)
        return (created, removed)
    }

    private func removeOwned(_ entry: MirrorManifest.Entry, root: URL) -> Int {
        let dir = root.appendingPathComponent(entry.dir, isDirectory: true)
        var n = 0
        for (name, target) in entry.links {
            let url = dir.appendingPathComponent(name)
            guard fs.kind(url) == .symlink, (try? fs.symlinkDestination(url)) == target else { continue }
            if (try? removeLink(url, in: dir)) != nil { n += 1 }
        }
        return n
    }

    /// The only delete path: `url` must be a symlink directly inside `dir`, and
    /// `dir` a real directory (not a symlink) directly inside the root.
    func removeLink(_ url: URL, in dir: URL) throws {
        let parent = url.deletingLastPathComponent().standardizedFileURL.path
        let dirPath = dir.standardizedFileURL.path
        guard fs.kind(url) == .symlink, parent == dirPath, fs.kind(dir) == .directory,
              dir.deletingLastPathComponent().standardizedFileURL.path == canonicalRoot.standardizedFileURL.path,
              fs.kind(canonicalRoot) == .directory
        else { throw MirrorError.refused(url.path) }
        try fs.removeLink(url)
    }

    // MARK: - Manifest

    private func loadManifest(_ root: URL) -> MirrorManifest {
        guard let data = fs.read(root.appendingPathComponent(Self.manifestName))
                ?? fs.read(root.appendingPathComponent(Self.legacyManifestName)),
              let m = try? JSONDecoder().decode(MirrorManifest.self, from: data) else { return MirrorManifest() }
        return m
    }

    private func saveManifest(_ m: MirrorManifest, root: URL) throws {
        let enc = JSONEncoder()
        enc.outputFormatting = [.sortedKeys, .prettyPrinted]
        try fs.write(try enc.encode(m), to: root.appendingPathComponent(Self.manifestName))
    }

    static func describe(_ error: Error) -> String {
        switch error {
        case MirrorError.refused(let p): "refused \(p) (a symlink or not a folder)"
        case MirrorError.io(let p, let e): "\(p): \(String(cString: strerror(e)))"
        default: error.localizedDescription
        }
    }
}
