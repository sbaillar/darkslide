import AppKit
import Foundation
import Security

/// Fetches the releases list (seam for tests).
public protocol ReleaseFetching: Sendable {
    func releases() async throws -> Data
}

/// `GET` the GitHub releases API, unauthenticated. Nothing identifies the user: the request
/// carries only `User-Agent: Darkslide/<version>`. https only (tests add "file").
public struct GitHubReleaseFetcher: ReleaseFetching {
    let url: URL
    let version: String
    let schemes: Set<String>

    public init(version: String, url: URL = ReleaseFeed.url) {
        self.init(version: version, url: url, schemes: ["https"])
    }

    init(version: String, url: URL, schemes: Set<String>) {
        self.url = url
        self.version = version
        self.schemes = schemes
    }

    public func releases() async throws -> Data {
        guard schemes.contains(url.scheme?.lowercased() ?? "") else {
            throw UpdateError("only https is allowed for the update check")
        }
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 30)
        request.setValue("Darkslide/\(version)", forHTTPHeaderField: "User-Agent")
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        let (data, response) = try await URLSession(configuration: .ephemeral).data(for: request)
        if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw UpdateError("GitHub answered HTTP \(http.statusCode)")
        }
        return data
    }
}

/// Why an update check, download or install stopped (said as is in the update sheet).
public struct UpdateError: Error, Equatable, LocalizedError {
    public var message: String
    public init(_ message: String) { self.message = message }
    public var errorDescription: String? { message }
}

/// Who signed an app, as far as an update needs to know: an update must be signed the
/// way the running app is, so a valid signature by someone else is refused.
public enum CodeSigner: Equatable, Sendable {
    /// A Developer ID (or other Apple-issued) identity: its team.
    case team(String)
    /// A certificate without a team, such as a self-signed "Darkslide Dev": its leaf certificate.
    case certificate(Data)
    /// Ad hoc (or unsigned): no identity to compare.
    case adHoc

    /// Whether an update signed by `update` may replace an app signed like this. An ad-hoc
    /// app has no identity to pin, so any valid signature passes (the download already
    /// matched GitHub's digest); otherwise the same team or certificate.
    public func accepts(_ update: CodeSigner) -> Bool {
        self == .adHoc || self == update
    }

    /// The signer of the bundle (or binary) at `url`.
    public static func of(_ url: URL) throws -> CodeSigner {
        var code: SecStaticCode?
        guard SecStaticCodeCreateWithPath(url as CFURL, [], &code) == errSecSuccess, let code else {
            throw UpdateError("“\(url.lastPathComponent)” isn’t an app Darkslide can check.")
        }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(code, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess,
              let dict = info as? [String: Any] else {
            throw UpdateError("The signature of “\(url.lastPathComponent)” couldn’t be read.")
        }
        return signer(team: dict[kSecCodeInfoTeamIdentifier as String] as? String,
                      leaf: (dict[kSecCodeInfoCertificates as String] as? [SecCertificate])?.first
                          .map { SecCertificateCopyData($0) as Data })
    }

    static func signer(team: String?, leaf: Data?) -> CodeSigner {
        if let team, !team.isEmpty { return .team(team) }
        if let leaf { return .certificate(leaf) }
        return .adHoc
    }
}

/// The updater's contact with the system: unpacking, the signature check, the relauncher,
/// Finder and quitting. Tests replace the parts that would touch the running app.
public struct UpdateTools: Sendable {
    public var unzip: @Sendable (_ zip: URL, _ into: URL) async throws -> Void
    public var verifySignature: @Sendable (_ app: URL) async throws -> Void
    /// Starts `/bin/sh -c script` with `arguments` ($1…), detached: it outlives the app.
    public var spawn: @Sendable (_ script: String, _ arguments: [String]) throws -> Void
    public var reveal: @MainActor @Sendable (URL) -> Void
    /// Quit through the normal path (`applicationShouldTerminate`: saves flush, running
    /// imports and exports are asked about).
    public var terminate: @MainActor @Sendable () -> Void
    /// Who signed a bundle (the running app, the downloaded one).
    public var signer: @Sendable (_ bundle: URL) throws -> CodeSigner = CodeSigner.of

    public static let live = UpdateTools(
        unzip: { zip, into in
            let (status, err) = try await run("/usr/bin/ditto", ["-x", "-k", zip.path, into.path])
            guard status == 0 else { throw UpdateError("The download couldn’t be unpacked: \(err)") }
        },
        verifySignature: { app in
            let (status, err) = try await run("/usr/bin/codesign", ["--verify", "--deep", "--strict", app.path])
            guard status == 0 else { throw UpdateError("The downloaded app’s signature doesn’t check out: \(err)") }
        },
        spawn: { script, arguments in
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/bin/sh")
            p.arguments = ["-c", script, "darkslide-update"] + arguments
            p.standardInput = FileHandle.nullDevice
            p.standardOutput = FileHandle.nullDevice
            p.standardError = FileHandle.nullDevice
            try p.run()
        },
        reveal: { NSWorkspace.shared.activateFileViewerSelecting([$0]) },
        terminate: { NSApp.terminate(nil) })

    /// Runs `executable` to the end; its exit status and what it wrote to stderr (trimmed).
    static func run(_ executable: String, _ arguments: [String]) async throws -> (Int32, String) {
        try await Task.detached {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: executable)
            p.arguments = arguments
            let err = Pipe()
            p.standardInput = FileHandle.nullDevice
            p.standardOutput = FileHandle.nullDevice
            p.standardError = err
            try p.run()
            let data = err.fileHandleForReading.readDataToEndOfFile() // EOF when it exits
            p.waitUntilExit()
            let text = String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
            return (p.terminationStatus, text)
        }.value
    }

    /// The relauncher, run by `/bin/sh -c` once Darkslide has quit:
    /// `$1` the app's pid, `$2` the installed bundle, `$3` the verified new bundle, `$4` where
    /// the old one goes ("Previous Version", replacing an older one), `$5` 1 to reopen the app,
    /// `$6` the program that reopens it (`/usr/bin/open`). It waits for the pid to exit (60 s
    /// at most), then:
    /// 1. moves the new bundle next to the old one (a copy when it is on another volume; the old
    ///    app is untouched until that is done);
    /// 2. renames the old bundle aside and the new one to its exact path, two renames in one
    ///    folder; if the second fails, the old bundle is renamed back;
    /// 3. moves the old bundle to `$4`. If that fails it stays next to the new one, hidden: the
    ///    previous version is never deleted before the new one is in place.
    /// Paths are arguments, never spliced into the script.
    public static let relauncher = """
        pid="$1"; old="$2"; new="$3"; backup="$4"; relaunch="$5"; opener="$6"
        [ -n "$pid" ] && [ -n "$old" ] && [ -n "$new" ] && [ -n "$backup" ] && [ -n "$opener" ] || exit 2
        for p in "$old" "$new" "$backup"; do case "$p" in *.app) ;; *) exit 2 ;; esac; done
        i=0
        while kill -0 "$pid" 2>/dev/null; do
          i=$((i + 1)); [ "$i" -gt 600 ] && exit 3
          sleep 0.1
        done
        reopen() { [ "$relaunch" = 1 ] && "$opener" "$old"; exit "$1"; }
        [ -d "$new" ] && [ -d "$old" ] || reopen 4
        dir="$(dirname "$old")"; name="$(basename "$old")"
        staged="$dir/.$name.update-$$"; aside="$dir/.$name.previous-$$"
        [ -e "$staged" ] || [ -e "$aside" ] && reopen 4
        if ! mv -- "$new" "$staged"; then rm -rf -- "$staged"; reopen 6; fi
        if ! mv -- "$old" "$aside"; then rm -rf -- "$staged"; reopen 5; fi
        if ! mv -- "$staged" "$old"; then
          mv -- "$aside" "$old"
          rm -rf -- "$staged"
          reopen 6
        fi
        mkdir -p "$(dirname "$backup")" && { [ ! -e "$backup" ] || rm -rf -- "$backup"; } && mv -- "$aside" "$backup"
        reopen 0
        """
}
