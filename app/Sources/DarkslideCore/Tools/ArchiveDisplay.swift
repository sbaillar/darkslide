import Foundation

/// Human text and SF Symbol for an archive state (shared by panel and toolbar badge).
public enum ArchiveDisplay {
    public static func title(_ s: ArchiveStatus.State) -> String {
        switch s {
        case .idle: "Archive up to date"
        case .syncing(let f): f < 0 ? "Syncing…" : "Syncing… \(Safe.clampedInt((f * 100))) %"
        case .behind(let n): "Behind by \(n) file\(n == 1 ? "" : "s")"
        case .problems: "Archived with problems"
        case .offline: "Archive offline"
        case .failed(let why): why.hasPrefix(RustArchiveService.settingsErrorPrefix) ? "Archive settings not applied" : "Last sync failed"
        case .notArchived: "Not archived yet"
        case .unavailable(let m): m
        }
    }

    public static func symbol(_ s: ArchiveStatus.State) -> String {
        switch s {
        case .idle: "checkmark.icloud"
        case .syncing: "arrow.triangle.2.circlepath.icloud"
        case .behind: "icloud.and.arrow.up"
        case .problems: "exclamationmark.icloud"
        case .offline: "icloud.slash"
        case .failed: "exclamationmark.icloud"
        case .notArchived: "icloud.and.arrow.up"
        case .unavailable: "icloud.slash"
        }
    }

    /// Toolbar badge text.
    public static func short(_ s: ArchiveStatus.State) -> String {
        switch s {
        case .idle: "Archived"
        case .syncing(let f): f < 0 ? "Syncing" : "\(Int(f * 100))%"
        case .behind(let n): "\(n) behind"
        case .problems: "Problems"
        case .offline: "Offline"
        case .failed: "Error"
        case .notArchived: "Not archived"
        case .unavailable: "Archive off"
        }
    }

    /// Popover status line.
    public static func long(_ s: ArchiveStatus.State) -> String {
        switch s {
        case .idle: "Up to date"
        case .syncing(let f): f < 0 ? "Syncing" : "Syncing, \(Int(f * 100))%"
        case .behind(let n): "\(n) file\(n == 1 ? "" : "s") waiting"
        case .problems(let n): "\(n) item\(n == 1 ? " is" : "s are") not in the archive. Retrying."
        case .offline: ArchiveDisplay.offlineText
        case .failed(let m): m
        case .notArchived: "A session hasn't been archived yet. It is archived with its next change or Sync Now."
        case .unavailable(let m): m
        }
    }

    /// Offline covers a share that isn't mounted and a different disk at its path (N3).
    public static let offlineText = "Target not available (not mounted, or a different disk is at that path). Changes are queued."

    /// N2: said with the badge (tooltip) and the popover title when the archive
    /// leaves rejected photos out, e.g. "3 rejects excluded".
    public static func exclusionNote(_ status: ArchiveStatus, settings: ArchiveSettingsSnapshot) -> String? {
        guard settings.isActive, !settings.archiveRejected else { return nil }
        let n = status.detail?.sessions.reduce(0) { $0 + $1.rejectsExcluded } ?? 0
        return n > 0 ? "\(n) reject\(n == 1 ? "" : "s") excluded" : "Rejected photos excluded"
    }

    /// The badge tooltip: the explanation, plus what is left out.
    public static func badgeHelp(_ status: ArchiveStatus, settings: ArchiveSettingsSnapshot) -> String {
        [explanation(status.state), exclusionNote(status, settings: settings)].compactMap { $0 }.joined(separator: "\n")
    }

    /// Popover explanation.
    public static func explanation(_ s: ArchiveStatus.State) -> String {
        if case .unavailable = s {
            return "Set an archive target and switch archiving on in Settings › Archive. Nothing is copied until then."
        }
        return "rsync copies sessions to the archive in the background. It never deletes from the archive; "
            + "replaced files are kept in .ds-archive-trash."
    }

    /// Why archiving is off (badge popover and state).
    public static func offText(_ s: ArchiveSettingsSnapshot) -> String {
        if s.target.trimmingCharacters(in: .whitespaces).isEmpty { return "Archive off: no target set" }
        return s.enabled ? "Archive off" : "Archive switched off"
    }

    // MARK: Popover detail

    /// One session line, e.g. "3 files waiting · last sync 2 min ago".
    public static func sessionLine(_ x: ArchiveDetail.Session, now: Date = Date()) -> String {
        if let why = x.notArchivable { return "Not archived: \(why)" }
        if x.notResponding { return "Not responding: the archive volume stopped answering. Retried when it's back." }
        if !x.problems.isEmpty {
            let n = x.problems.count
            var s = "\(n) item\(n == 1 ? "" : "s") not archived"
            if let r = x.retryIn { s += " · retry in \(Formatters.duration(Double(r.components.seconds)))" }
            if let d = x.lastSuccess { s += " · last full sync \(Formatters.relative(d, now: now))" }
            return s
        }
        if x.running {
            let pct = x.fraction.map { " \(Safe.clampedInt(($0 * 100))) %" } ?? ""
            let rate = x.bytesPerSecond.map { " · \(ByteCountFormatter.string(fromByteCount: Int64($0), countStyle: .file))/s" } ?? ""
            return "Syncing\(pct)\(rate)"
        }
        var parts: [String] = []
        if let e = x.lastError {
            parts.append("Last sync failed: \(e)")
            if let r = x.retryIn { parts.append("retry in \(Formatters.duration(Double(r.components.seconds)))") }
            if x.lastSuccess == nil { parts.append("never archived") }
        } else if x.pending > 0 {
            parts.append("\(x.pending) change\(x.pending == 1 ? "" : "s") waiting")
        } else if x.lastSuccess != nil {
            parts.append("Up to date")
            if let counts = archivedCounts(x) { parts.append(counts) }
        } else {
            parts.append("Not archived yet")
        }
        if let d = x.lastSuccess { parts.append("last sync \(Formatters.relative(d, now: now))") }
        return parts.joined(separator: " · ")
    }

    /// N2: what the last run put in the archive, e.g. "8 of 8 archived" or
    /// "5 archived, 3 rejects excluded" (`nil` before the first counted run).
    public static func archivedCounts(_ x: ArchiveDetail.Session) -> String? {
        guard x.originals > 0 else { return nil }
        let archived = x.originals - x.rejectsExcluded
        if x.rejectsExcluded == 0 { return "\(archived) of \(x.originals) archived" }
        return "\(archived) archived, \(x.rejectsExcluded) reject\(x.rejectsExcluded == 1 ? "" : "s") excluded"
    }

    /// One run line, e.g. "Trip · on demand · succeeded in 1.2 s".
    public static func runLine(_ r: ArchiveDetail.Run) -> String {
        let secs = Double(r.duration.components.seconds) + Double(r.duration.components.attoseconds) / 1e18
        let took = secs < 10 ? String(format: "%.1f s", secs) : "\(Safe.clampedInt(secs)) s"
        var s = "\(r.session) · \(r.trigger) · \(r.outcome)"
        if ["succeeded", "failed", "incomplete"].contains(r.outcome) { s += " in \(took)" }
        if !r.problems.isEmpty { s += ": \(r.problems.count) not archived" }
        if let m = r.message { s += ": \(m)" }
        if !r.warnings.isEmpty { s += " (\(r.warnings.count) warning\(r.warnings.count == 1 ? "" : "s"))" }
        return s
    }

    /// Amazon Photos hand-off line.
    public static func amazonLine(_ a: ArchiveDetail.Amazon) -> String {
        if a.running { return "Handing originals to Amazon Photos…" }
        if a.folderMissing { return "Amazon Photos folder missing: \(a.folder). Choose it again in Settings › Archive." }
        if let e = a.error { return "Amazon Photos: \(e)" }
        guard a.lastRun != nil else { return "Amazon Photos: after the next import" }
        // L2: the cumulative total, not "0 handed over, 40 already there" after the first pass.
        let total = a.placed + a.alreadyMirrored
        var parts = ["\(total) original\(total == 1 ? "" : "s") handed to the app" + (a.placed > 0 ? " (\(a.placed) new)" : "")]
        if a.rejectsExcluded > 0 { parts.append("\(a.rejectsExcluded) reject\(a.rejectsExcluded == 1 ? "" : "s") excluded") }
        if a.waiting > 0 { parts.append("\(a.waiting) waiting for verification") }
        if !a.failed.isEmpty { parts.append("\(a.failed.count) failed") }
        return "Amazon Photos: " + parts.joined(separator: ", ")
    }

    /// Whether a run line shows as a problem (red) in the popover.
    public static func isProblem(_ r: ArchiveDetail.Run) -> Bool {
        r.outcome != "succeeded" && r.outcome != "cancelled" && r.outcome != "busy"
    }

    public static func canSyncNow(_ s: ArchiveStatus.State) -> Bool {
        // Offline: "Sync Now" still queues it for when the target is back.
        switch s {
        case .syncing, .unavailable: false
        default: true
        }
    }

    public static func accessibilityLabel(_ status: ArchiveStatus) -> String {
        "Archive: \(long(status.state))"
    }
}
