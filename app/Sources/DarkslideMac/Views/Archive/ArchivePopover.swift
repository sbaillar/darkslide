import DarkslideCore
import DarkslideUI
import SwiftUI

/// Archive details from the toolbar badge: target, per-session status, recent
/// runs, the Amazon Photos hand-off, and "Sync Now".
struct ArchivePopover: View {
    @Bindable var model: AppModel

    var body: some View {
        let status = model.archiveStatus
        let state = status.state
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack {
                VStack(alignment: .leading, spacing: 1) {
                    Label(ArchiveDisplay.title(state), systemImage: ArchiveDisplay.symbol(state)).font(.headline)
                    if let note = ArchiveDisplay.exclusionNote(status, settings: model.settings.archiveSnapshot) {
                        Text(note).font(DS.Font.caption).foregroundStyle(DS.textSecondary) // N2
                    }
                }
                Spacer()
                Button("Sync Now") { Task { await model.archive.syncNow() } }
                    .disabled(!ArchiveDisplay.canSyncNow(state))
                    .accessibilityHint("Archive every open session now")
            }
            LabeledContent("Target") {
                Text(status.target.isEmpty ? "Not set" : status.target).lineLimit(1).truncationMode(.middle)
            }
            .help(status.target)
            if let why = status.detail?.targetReason, state == .offline {
                Text(why).font(DS.Font.caption).foregroundStyle(DS.reject)
            }
            LabeledContent("Last sync", value: Formatters.relative(status.lastSuccess))
            if let rsync = status.detail?.rsync {
                LabeledContent("Using") { Text(rsync).lineLimit(1).truncationMode(.middle) }
                    .font(DS.Font.caption)
            }
            if let sessions = status.detail?.sessions, !sessions.isEmpty {
                Divider()
                Text("Sessions").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                ForEach(sessions) { s in
                    VStack(alignment: .leading, spacing: 1) {
                        Text(s.name).font(DS.Font.label)
                        Text(ArchiveDisplay.sessionLine(s)).font(DS.Font.caption)
                            .foregroundStyle(s.problems.isEmpty && !s.notResponding && s.lastError == nil ? DS.textSecondary : DS.reject)
                            .fixedSize(horizontal: false, vertical: true)
                        if let raw = s.errorDetail { ArchiveDetailsDisclosure(raw: raw) }
                        ForEach(s.problems.prefix(5), id: \.self) { p in
                            Text(p).font(DS.Font.caption).foregroundStyle(DS.reject).lineLimit(2).truncationMode(.middle)
                        }
                        if s.problems.count > 5 {
                            Text("and \(s.problems.count - 5) more").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                        }
                        if let a = s.amazon {
                            Text(ArchiveDisplay.amazonLine(a)).font(DS.Font.caption)
                                .foregroundStyle(a.error == nil ? DS.textSecondary : DS.reject)
                        }
                    }
                    .accessibilityElement(children: .combine)
                }
            }
            if let runs = status.detail?.runs, !runs.isEmpty {
                Divider()
                Text("Recent runs").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                ForEach(runs.prefix(5)) { r in
                    VStack(alignment: .leading, spacing: 1) {
                        Label(ArchiveDisplay.runLine(r), systemImage: ArchiveDisplay.isProblem(r) ? "exclamationmark.circle" : "checkmark.circle")
                            .font(DS.Font.caption)
                            .foregroundStyle(ArchiveDisplay.isProblem(r) ? DS.reject : DS.textSecondary)
                            .lineLimit(2)
                        if let raw = r.detail { ArchiveDetailsDisclosure(raw: raw) }
                    }
                }
            }
            if let amazon = status.detail?.amazon {
                Divider()
                Label(ArchiveDisplay.amazonLine(amazon), systemImage: "icloud.and.arrow.up")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                    .help(amazon.folder)
            }
            Text(ArchiveDisplay.explanation(state))
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(DS.Font.label)
        .padding(DS.Space.m)
        .frame(width: 360)
    }
}

/// N1c: rsync's own words, only on request.
struct ArchiveDetailsDisclosure: View {
    let raw: String

    var body: some View {
        DisclosureGroup("Details") {
            Text(raw).font(.caption.monospaced()).textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(DS.Font.caption)
    }
}
