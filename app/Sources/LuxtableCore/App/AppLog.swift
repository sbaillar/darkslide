import os

/// Unified logging (`log show --predicate 'subsystem == "app.luxtable.Luxtable"'`).
/// One line per failure and per lifecycle step, so a problem a user reports can be
/// diagnosed afterwards. Paths, file names, hosts and error text are private with a hash
/// mask (`.private(mask: .hash)`: equal values match across lines without revealing them);
/// counts and states are public.
public enum AppLog {
    public static let subsystem = "app.luxtable.Luxtable"
    /// Develop renders: failures, retries, missing originals.
    public static let render = Logger(subsystem: subsystem, category: "render")
    /// Sessions and recent folders: open, close, relocate, recents skipped.
    public static let session = Logger(subsystem: subsystem, category: "session")
    /// Card and folder imports: start, finish, failures.
    public static let `import` = Logger(subsystem: subsystem, category: "import")
    /// Archive runs: start, result, failures.
    public static let archive = Logger(subsystem: subsystem, category: "archive")
    /// Natural-language search: indexing failures, global-index problems.
    public static let search = Logger(subsystem: subsystem, category: "search")
    /// The first launch after the rename (Darkslide → Luxtable): what was copied.
    public static let migration = Logger(subsystem: subsystem, category: "migration")
}
