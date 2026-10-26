import Foundation

/// Files a folder open couldn't use, and damaged RAWs (final-journeys M2).
public enum OpenFailures {
    /// What lt-ffi says about a RAW file that is damaged or incomplete.
    public static let damagedMessage = "This RAW file is damaged or incomplete — copy it from the card again."

    /// The open result, naming the files: "Couldn't read EMPTY.ARW." / "… and 3 more."
    public static func message(_ urls: [URL], shown: Int = 3) -> String {
        let names = urls.map(\.lastPathComponent)
        guard !names.isEmpty else { return "" }
        let head = names.prefix(shown).joined(separator: ", ")
        let rest = names.count - min(shown, names.count)
        let tail = rest > 0 ? " and \(rest) more" : ""
        return "Couldn’t read \(head)\(tail): damaged, incomplete or unreadable. Copy \(names.count == 1 ? "it" : "them") from the card again."
    }

    /// A render or preview failed because the original file isn't there (deleted,
    /// moved, or its drive unmounted).
    public static func isMissingFile(_ error: Error) -> Bool {
        if let e = error as? CocoaError, e.code == .fileNoSuchFile || e.code == .fileReadNoSuchFile { return true }
        if let e = error as? POSIXError, e.code == .ENOENT { return true }
        let ns = error as NSError
        if ns.domain == NSPOSIXErrorDomain, ns.code == Int(ENOENT) { return true }
        let text = (error as? RenderError)?.message ?? error.localizedDescription
        return text.contains("No such file or directory") || text.contains("os error 2)")
    }

    /// A develop failed because the RAW is a format or sensor Luxtable can't develop
    /// yet (lt-raw's `Unsupported`: "unsupported camera …" / "unsupported file: …"),
    /// not because it's damaged, missing or not a RAW (those are `.unsupported` too).
    public static func isUnsupportedFormat(_ error: Error) -> Bool {
        guard let e = error as? RenderError, e.kind == .unsupported else { return false }
        return e.message.hasPrefix("unsupported camera ") || e.message.hasPrefix("unsupported file: ")
    }

    /// A render or preview failed because the RAW is damaged.
    public static func isDamaged(_ error: Error) -> Bool {
        (error as? RenderError)?.message == damagedMessage || error.localizedDescription == damagedMessage
    }
}

/// [cul9] L6: what VoiceOver says for the histogram: its brightness and clipping.
public enum HistogramText {
    public static func value(_ h: Histogram?) -> String {
        guard let h, h.pixelCount > 0 else { return "No image" }
        let total = Double(h.pixelCount)
        var sum = 0.0
        for (i, n) in h.luma.enumerated() { sum += Double(i) * Double(n) }
        let mean = Int((sum / total / 255 * 100).rounded())
        let shadows = Double(h.shadowClipped) / total * 100
        let highlights = Double(h.highlightClipped) / total * 100
        let tone = mean < 35 ? "dark" : (mean > 65 ? "bright" : "mid-tone")
        return String(format: "Mostly %@, average brightness %d%%; %.1f%% clipped shadows, %.1f%% clipped highlights",
                      tone, mean, shadows, highlights)
    }
}

/// Accessibility and badge wording for ratings (L6).
public enum RatingText {
    /// "1 star", "3 stars", "Unrated".
    public static func label(_ rating: Int) -> String {
        rating <= 0 ? "Unrated" : "\(rating) \(rating == 1 ? "star" : "stars")"
    }

    /// A click on star `n` of a photo rated `current`: the same star again clears it.
    public static func rating(afterClicking n: Int, current: Int) -> Int {
        n == current ? 0 : min(max(n, 0), 5)
    }
}
