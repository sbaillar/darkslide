import Foundation

/// Photo-specific value formatting (pure, unit-tested).
public enum Formatters {
    /// 1/250, 1/4000, 0.5", 2", 30".
    public static func shutter(_ seconds: Double) -> String {
        guard seconds > 0, seconds.isFinite else { return "–" }
        if seconds >= 0.3 {
            let v = seconds.rounded() == seconds ? String(Safe.clampedInt(seconds)) : String(format: "%.1f", seconds)
            return "\(v)\""
        }
        return "1/\(Safe.clampedInt(1 / seconds))"
    }

    /// f/2.8, f/8, f/11.
    public static func aperture(_ f: Double) -> String {
        guard f > 0, f.isFinite else { return "–" }
        return f.rounded() == f && f < 1000 ? "f/\(Int(f))" : String(format: "f/%.1f", f)
    }

    public static func focal(_ mm: Double) -> String {
        "\(Safe.int(mm).map(String.init) ?? "–") mm"
    }

    /// Signed with fixed decimals: +0.30, -1.00, 0.00.
    public static func signed(_ v: Double, decimals: Int = 2) -> String {
        let s = String(format: "%.\(decimals)f", abs(v))
        let isZero = Double(s) == 0
        return isZero ? s : (v > 0 ? "+" : "-") + s
    }

    /// 48.5 MB, 1.2 GB (decimal, like Finder).
    public static func bytes(_ n: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: n, countStyle: .file)
    }

    /// 285 MB/s.
    public static func throughput(bytesPerSecond: Double) -> String {
        guard bytesPerSecond.isFinite, bytesPerSecond > 0 else { return "–" }
        return "\(ByteCountFormatter.string(fromByteCount: Int64(min(bytesPerSecond, 1e15)), countStyle: .file))/s"
    }

    /// 45 s, 2 min 05 s, 1 h 03 min.
    public static func duration(_ seconds: Double) -> String {
        guard seconds.isFinite, seconds >= 0 else { return "–" }
        let s = Safe.clampedInt(seconds)
        if s < 60 { return "\(s) s" }
        if s < 3600 { return String(format: "%d min %02d s", s / 60, s % 60) }
        return String(format: "%d h %02d min", s / 3600, (s % 3600) / 60)
    }

    /// Kelvin: 5,500 K.
    public static func kelvin(_ k: Double) -> String {
        "\(Safe.int(k)?.formatted() ?? "–") K"
    }

    /// "2 min ago" style for archive status.
    public static func relative(_ date: Date?, now: Date = Date()) -> String {
        guard let date else { return "never" }
        let d = now.timeIntervalSince(date)
        if d < 60 { return "just now" }
        if d < 3600 { return "\(Int(d / 60)) min ago" }
        if d < 86_400 { return "\(Int(d / 3600)) h ago" }
        return "\(Int(d / 86_400)) d ago"
    }
}
