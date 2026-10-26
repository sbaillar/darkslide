import LuxtableCore
import LuxtableUI
import SwiftUI

/// What an import from another application did or would do: the summary lines, the
/// edits by tool, conflicts, what isn't mapped, the folders of photos that aren't in
/// this session (each with a button that opens it as a session), what was left out
/// and per-photo notes. Shared by the Lightroom and the Aperture / iPhoto imports.
struct ImportReportView: View {
    let summary: LRImportSummary
    let folders: [LightroomImportModel.FolderRow]
    let openAsSession: (String) async -> Void

    var body: some View {
        let s = summary
        ScrollView {
            VStack(alignment: .leading, spacing: DS.Space.s) {
                Text(s.matchLine).font(DS.Font.label).foregroundStyle(DS.textSecondary)
                section(nil, s.lines)
                section("Edits by tool", s.toolLines)
                section("Conflicts: changed in Luxtable since the last import", s.conflictLines, warning: true)
                section("Not mapped (no Luxtable equivalent)", s.notMappedLines)
                folderList
                section("Left out", s.leftOutLines)
                if !s.noteLines.isEmpty {
                    DisclosureGroup("Notes per photo (\(s.noteLines.count))") {
                        VStack(alignment: .leading, spacing: DS.Space.xxs) {
                            ForEach(Array(s.noteLines.enumerated()), id: \.offset) { _, l in
                                Text(l).font(DS.Font.caption).foregroundStyle(DS.text).textSelection(.enabled)
                            }
                        }
                    }
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxHeight: 300)
    }

    @ViewBuilder private var folderList: some View {
        let rows = folders
        if !rows.isEmpty {
            VStack(alignment: .leading, spacing: DS.Space.xxs) {
                Text("NOT IN THIS SESSION").font(DS.Font.caption).tracking(0.5).foregroundStyle(DS.textSecondary)
                    .accessibilityLabel("Not in this session").accessibilityAddTraits(.isHeader)
                ForEach(rows.prefix(12), id: \.path) { row in
                    HStack {
                        Text("\(row.path): \(LRImportSummary.photos(row.photos))").font(DS.Font.label).foregroundStyle(DS.text)
                            .lineLimit(1).truncationMode(.middle)
                        Spacer()
                        switch row.state {
                        case .inSession(let name): Text("in \u{201c}\(name)\u{201d}").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                        case .missing: Text("folder not found").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                        case .available: Button("Open as Session\u{2026}") { Task { await openAsSession(row.path) } }
                        }
                    }
                }
                if rows.count > 12 { Text("and \(rows.count - 12) more folders").font(DS.Font.caption).foregroundStyle(DS.textSecondary) }
            }
        }
    }

    @ViewBuilder private func section(_ title: String?, _ lines: [String], warning: Bool = false) -> some View {
        if !lines.isEmpty {
            VStack(alignment: .leading, spacing: DS.Space.xxs) {
                if let title {
                    Text(title.uppercased()).font(DS.Font.caption).tracking(0.5)
                        .foregroundStyle(warning ? DS.warningText : DS.textSecondary)
                        .accessibilityLabel(title)
                        .accessibilityAddTraits(.isHeader)
                }
                ForEach(Array(lines.enumerated()), id: \.offset) { _, l in
                    Text(l).font(DS.Font.label).foregroundStyle(DS.text).textSelection(.enabled)
                }
            }
        }
    }
}
