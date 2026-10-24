import Foundation

/// A Darkslide version: `MAJOR.MINOR.PATCH` with an optional semver pre-release
/// (`0.1.19-beta.2`), from a tag (`v…`) or `CFBundleShortVersionString`. Ordered by semver:
/// a pre-release comes before its release, and numeric identifiers compare as numbers.
public struct AppVersion: Comparable, Hashable, Sendable, CustomStringConvertible {
    public var major: Int
    public var minor: Int
    public var patch: Int
    /// The dot-separated pre-release identifiers (`["beta", "2"]`); empty for a release.
    public var prerelease: [String]

    public init?(_ text: String) {
        var s = Substring(text.trimmingCharacters(in: .whitespaces))
        if s.first == "v" { s = s.dropFirst() }
        let parts = s.split(separator: "-", maxSplits: 1, omittingEmptySubsequences: false)
        let core = parts[0].split(separator: ".", omittingEmptySubsequences: false)
        guard core.count == 3, let a = Self.number(core[0]), let b = Self.number(core[1]), let c = Self.number(core[2])
        else { return nil }
        var pre: [String] = []
        if parts.count == 2 {
            pre = parts[1].split(separator: ".", omittingEmptySubsequences: false).map(String.init)
            guard pre.allSatisfy({ !$0.isEmpty && $0.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") } })
            else { return nil }
        }
        (major, minor, patch, prerelease) = (a, b, c, pre)
    }

    private static func number(_ s: Substring) -> Int? {
        guard !s.isEmpty, s.allSatisfy({ $0.isASCII && $0.isNumber }) else { return nil }
        return Int(s)
    }

    public var isPrerelease: Bool { !prerelease.isEmpty }

    public var description: String {
        "\(major).\(minor).\(patch)" + (prerelease.isEmpty ? "" : "-" + prerelease.joined(separator: "."))
    }

    public static func < (a: AppVersion, b: AppVersion) -> Bool {
        if (a.major, a.minor, a.patch) != (b.major, b.minor, b.patch) {
            return (a.major, a.minor, a.patch) < (b.major, b.minor, b.patch)
        }
        switch (a.prerelease.isEmpty, b.prerelease.isEmpty) {
        case (true, true), (true, false): return false
        case (false, true): return true
        case (false, false): break
        }
        for (x, y) in zip(a.prerelease, b.prerelease) where x != y {
            switch (Int(x), Int(y)) {
            case let (i?, j?): return i < j
            case (.some, nil): return true // numeric identifiers sort first
            case (nil, .some): return false
            case (nil, nil): return x < y
            }
        }
        return a.prerelease.count < b.prerelease.count
    }
}

/// An app release Darkslide can update to: its version, notes and the verified download.
public struct UpdateRelease: Equatable, Sendable, Identifiable {
    public var version: AppVersion
    public var isPrerelease: Bool
    /// The short release notes: the `## What's new` bullets, as plain text.
    public var notes: [String]
    public var page: URL?
    public var asset: Asset

    public struct Asset: Equatable, Sendable {
        public var name: String
        public var url: URL
        public var size: Int64
        /// Lowercase hex, from GitHub's `digest` ("sha256:<hex>").
        public var sha256: String
    }

    public var id: String { version.description }

    /// The macOS app's asset name for `version` (scripts and docs/RELEASING.md use the same).
    public static func assetName(_ version: AppVersion) -> String { "Darkslide-\(version)-macos-arm64.zip" }
}

/// The GitHub releases API's JSON, and which release is an update.
public enum ReleaseFeed {
    /// Unauthenticated; only the app version is sent (the User-Agent).
    public static let url = URL(string: "https://api.github.com/repos/sbaillar/darkslide/releases?per_page=20")!

    struct Release: Decodable {
        var tag_name: String
        var prerelease: Bool
        var draft: Bool
        var body: String?
        var html_url: String?
        var assets: [Asset]

        struct Asset: Decodable {
            var name: String
            var size: Int64
            var browser_download_url: String
            var digest: String?
        }
    }

    /// Every app release in `data`: tags `v<semver>` with the macOS asset, an https URL and a
    /// SHA-256 digest. Drafts and anything else (the RAW helper's `rawler-helper-N`) are left out.
    public static func releases(from data: Data) throws -> [UpdateRelease] {
        try JSONDecoder().decode([Release].self, from: data).compactMap { r in
            guard !r.draft, r.tag_name.hasPrefix("v"), let v = AppVersion(r.tag_name),
                  let a = r.assets.first(where: { $0.name == UpdateRelease.assetName(v) }),
                  let url = URL(string: a.browser_download_url), url.scheme == "https",
                  let sha = sha256(a.digest), a.size > 0 else { return nil }
            return UpdateRelease(version: v, isPrerelease: r.prerelease || v.isPrerelease,
                                 notes: ReleaseNotes.short(r.body ?? ""), page: r.html_url.flatMap(URL.init(string:)),
                                 asset: .init(name: a.name, url: url, size: a.size, sha256: sha))
        }
    }

    /// The newest release after `current` on the channel: betas only when `beta` is on.
    public static func newest(_ releases: [UpdateRelease], after current: AppVersion, beta: Bool) -> UpdateRelease? {
        releases.filter { (beta || !$0.isPrerelease) && $0.version > current }.max { $0.version < $1.version }
    }

    static func sha256(_ digest: String?) -> String? {
        guard let digest, digest.hasPrefix("sha256:") else { return nil }
        let hex = digest.dropFirst("sha256:".count).lowercased()
        return hex.count == 64 && hex.allSatisfy(\.isHexDigit) ? hex : nil
    }
}

/// The short release notes: a release body's `## What's new` section as plain bullets.
public enum ReleaseNotes {
    /// The bullets of the "What's new" section, markdown removed; without one, the body's
    /// first ten non-empty lines.
    public static func short(_ body: String) -> [String] {
        let lines = body.replacingOccurrences(of: "\r\n", with: "\n").components(separatedBy: "\n")
        guard let start = lines.firstIndex(where: isWhatsNew) else {
            return lines.map { plain(heading($0)) }.filter { !$0.isEmpty }.prefix(10).map { $0 }
        }
        var bullets: [String] = []
        for line in lines[(start + 1)...] {
            let t = line.trimmingCharacters(in: .whitespaces)
            if t.hasPrefix("#") { break }
            if t.hasPrefix("- ") || t.hasPrefix("* ") {
                bullets.append(String(t.dropFirst(2)))
            } else if !t.isEmpty {
                if bullets.isEmpty { bullets.append(t) } else { bullets[bullets.count - 1] += " " + t }
            }
        }
        return bullets.map(plain).filter { !$0.isEmpty }
    }

    static func isWhatsNew(_ line: String) -> Bool {
        let t = line.trimmingCharacters(in: .whitespaces)
        guard t.hasPrefix("#") else { return false }
        return heading(t).lowercased().replacingOccurrences(of: "’", with: "'") == "what's new"
    }

    private static func heading(_ line: String) -> String {
        String(line.trimmingCharacters(in: .whitespaces).drop { $0 == "#" }).trimmingCharacters(in: .whitespaces)
    }

    /// Markdown to plain text: links keep their text, emphasis and code marks go.
    static func plain(_ s: String) -> String {
        var out = s.replacingOccurrences(of: #"\[([^\]]*)\]\([^)]*\)"#, with: "$1", options: .regularExpression)
        for mark in ["**", "__", "`"] { out = out.replacingOccurrences(of: mark, with: "") }
        return out.trimmingCharacters(in: .whitespaces)
    }
}
