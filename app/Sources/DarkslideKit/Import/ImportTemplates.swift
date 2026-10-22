import Foundation

/// Session-name and file-rename templates (PLAN §5 step 3). Pure and unit-tested.
///
/// Tokens: `{original}` (filename without extension), `{date}`, `{time}`,
/// `{seq}` (001), `{seq4}` (0001), `{camera}`, `{text}`. Unknown tokens stay literal.
public enum ImportTemplates {
    public static let renamePresets = ["{original}", "{camera}_{date}_{seq4}", "{date}_{time}_{seq}", "{text}_{seq4}"]

    /// Session folder name, e.g. `"{date} {text}"` → `"2026-09-25 Fall Hike"`.
    public static func sessionName(_ template: String, date: Date, text: String, timeZone: TimeZone = .current) -> String {
        let raw = template
            .replacingOccurrences(of: "{date}", with: format(date, "yyyy-MM-dd", timeZone))
            .replacingOccurrences(of: "{time}", with: format(date, "HHmm", timeZone))
            .replacingOccurrences(of: "{text}", with: text.trimmingCharacters(in: .whitespaces))
        let collapsed = raw.split(separator: " ", omittingEmptySubsequences: true).joined(separator: " ")
        return sanitize(collapsed)
    }

    /// New filename (with the original extension) for the `seq`-th imported file (1-based).
    public static func rename(_ template: String, original: String, date: Date, seq: Int, camera: String, text: String,
                              timeZone: TimeZone = .current) -> String {
        let url = URL(fileURLWithPath: original)
        let ext = url.pathExtension
        let stem = url.deletingPathExtension().lastPathComponent
        let base = template
            .replacingOccurrences(of: "{original}", with: stem)
            .replacingOccurrences(of: "{date}", with: format(date, "yyyyMMdd", timeZone))
            .replacingOccurrences(of: "{time}", with: format(date, "HHmmss", timeZone))
            .replacingOccurrences(of: "{seq4}", with: String(format: "%04d", seq))
            .replacingOccurrences(of: "{seq}", with: String(format: "%03d", seq))
            .replacingOccurrences(of: "{camera}", with: camera.replacingOccurrences(of: " ", with: "-"))
            .replacingOccurrences(of: "{text}", with: text.trimmingCharacters(in: .whitespaces))
        let name = sanitize(base)
        return ext.isEmpty ? name : "\(name).\(ext)"
    }

    /// Replace path separators and characters Finder/SMB reject; never empty.
    public static func sanitize(_ s: String) -> String {
        let bad = CharacterSet(charactersIn: "/:\\?*\"<>|").union(.controlCharacters)
        let replaced = String(s.unicodeScalars.map { bad.contains($0) ? "-" : Character($0) })
        let trimmed = replaced.trimmingCharacters(in: CharacterSet(charactersIn: " .").union(.whitespacesAndNewlines))
        return trimmed.isEmpty ? "Untitled" : trimmed
    }

    static func format(_ date: Date, _ pattern: String, _ tz: TimeZone) -> String {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.timeZone = tz
        f.dateFormat = pattern
        return f.string(from: date)
    }
}
