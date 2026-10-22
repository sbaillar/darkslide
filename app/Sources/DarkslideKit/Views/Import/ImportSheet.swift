import AppKit
import SwiftUI

/// Memory-card import sheet (PLAN §5). Logic lives in `ImportModel`.
struct ImportSheet: View {
    @Bindable var model: AppModel
    @State private var importer: ImportModel
    @State private var thumbnails: ImportThumbnails

    init(model: AppModel, volume: URL?) {
        self.model = model
        _importer = State(initialValue: ImportModel(model: model, volume: volume))
        _thumbnails = State(initialValue: ImportThumbnails(previews: model.previews))
    }

    var body: some View {
        VStack(spacing: 0) {
            ImportHeader(importer: importer, chooseFolder: chooseFolder)
            Divider()
            if !importer.copyEnabled {
                LiveImportNotice(importer: importer, close: { model.sheet = nil })
                Divider()
            }
            content
            Divider()
            ImportFooter(importer: importer, close: { model.sheet = nil })
        }
        .frame(width: 920, height: 640)
        .background(DS.panelBackground)
        .task { await importer.findCardIfNeeded(source: WorkspaceVolumeEvents()) }
    }

    @ViewBuilder private var content: some View {
        switch importer.phase {
        case .idle:
            ImportEmptyState(title: "No card detected",
                             message: "Insert a memory card and it will appear here, or choose any folder that contains photos.",
                             symbol: "sdcard", action: ("Choose Folder…", chooseFolder))
        case .scanning:
            ProgressView("Reading card…").frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message) where importer.candidates.isEmpty:
            ImportEmptyState(title: "Couldn’t read the card", message: message, symbol: "exclamationmark.triangle",
                             action: ("Try Again", { importer.retry() }))
        case .finished(let summary):
            ImportDoneView(summary: summary, importer: importer)
        default:
            if importer.candidates.isEmpty {
                ImportEmptyState(title: "No photos found",
                                 message: "This location has no supported photos. Choose another folder.",
                                 symbol: "photo.on.rectangle.angled", action: ("Choose Folder…", chooseFolder))
            } else {
                HStack(spacing: 0) {
                    CandidateGrid(importer: importer, thumbnails: thumbnails)
                    Divider()
                    ImportOptions(importer: importer)
                        .frame(width: 300)
                }
            }
        }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.prompt = "Choose"
        panel.message = "Choose a memory card or a folder with photos"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        Task { await importer.setVolume(url) }
    }
}

private struct ImportHeader: View {
    let importer: ImportModel
    let chooseFolder: () -> Void

    var body: some View {
        HStack(spacing: DS.Space.m) {
            Image(systemName: importer.volume == nil ? "sdcard" : "sdcard.fill")
                .font(.system(size: 22))
                .foregroundStyle(DS.accent)
                .frame(width: 44, height: 44)
                .background(DS.accent.opacity(0.14), in: .rect(cornerRadius: DS.Radius.l))
            VStack(alignment: .leading, spacing: DS.Space.xxs) {
                Text("Import from \(importer.volumeName)").font(.system(size: 15, weight: .semibold))
                Text(subtitle).font(DS.Font.label).foregroundStyle(DS.textSecondary)
            }
            Spacer()
            Button("Choose Folder…", action: chooseFolder)
                .disabled(importer.isBusy)
                .help("Import from any folder instead of a card")
        }
        .padding(DS.Space.m)
    }

    private var subtitle: String {
        guard !importer.candidates.isEmpty else { return importer.volume?.path ?? "Waiting for a memory card" }
        let videos = importer.videoCount
        var s = "\(importer.candidates.count - videos) photos"
        if videos > 0 { s += " · \(videos) video\(videos == 1 ? "" : "s")" }
        s += " · \(Formatters.bytes(importer.totalBytes))"
        if importer.duplicateCount > 0 { s += " · \(importer.duplicateCount) already imported" }
        return s
    }
}

private struct CandidateGrid: View {
    let importer: ImportModel
    let thumbnails: ImportThumbnails

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: DS.Space.s) {
                Text("\(importer.selected.count) of \(importer.candidates.count) selected")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                Spacer()
                Button("Select All") { importer.selectAll() }
                Button("None") { importer.selectNone() }
            }
            .buttonStyle(.borderless)
            .font(DS.Font.label)
            .disabled(importer.isBusy)
            .padding(.horizontal, DS.Space.m)
            .padding(.vertical, DS.Space.s)
            ScrollView {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 112), spacing: DS.Space.s)], spacing: DS.Space.s) {
                    ForEach(importer.candidates) { c in
                        Button { importer.toggle(c) } label: {
                            CandidateTile(candidate: c, selected: importer.isSelected(c), locked: importer.isLocked(c),
                                          card: importer.volumeID, thumbnails: thumbnails)
                        }
                        .buttonStyle(.plain)
                        .disabled(importer.isLocked(c) || importer.isBusy)
                        .accessibilityLabel(ImportWording.tileLabel(c, selected: importer.isSelected(c), locked: importer.isLocked(c)))
                        .accessibilityAddTraits(importer.isSelected(c) ? .isSelected : [])
                    }
                }
                .padding(.horizontal, DS.Space.m)
                .padding(.bottom, DS.Space.m)
            }
        }
        .background(DS.viewerBackground)
    }
}

private struct CandidateTile: View {
    let candidate: ImportCandidate
    let selected: Bool
    let locked: Bool
    /// The card's volume id: thumbnails of another card never show here (C1).
    let card: String?
    let thumbnails: ImportThumbnails
    @State private var image: CGImage?

    var body: some View {
        VStack(spacing: DS.Space.xs) {
            ZStack(alignment: .topTrailing) {
                RoundedRectangle(cornerRadius: DS.Radius.s)
                    .fill(DS.cellBackground)
                    .overlay {
                        if let image {
                            Image(decorative: image, scale: 1).resizable().aspectRatio(contentMode: .fit)
                        } else {
                            Image(systemName: "photo")
                                .font(.system(size: 22, weight: .light))
                                .foregroundStyle(DS.textTertiary)
                        }
                    }
                    .aspectRatio(1.5, contentMode: .fit)
                    .clipShape(.rect(cornerRadius: DS.Radius.s))
                if candidate.kind == .video {
                    Image(systemName: "play.circle.fill")
                        .font(.system(size: 22))
                        .foregroundStyle(.white, .black.opacity(0.45))
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
                if candidate.alreadyImported {
                    Text("Imported")
                        .font(.system(size: 9, weight: .semibold))
                        .padding(.horizontal, 5).padding(.vertical, 2)
                        .background(.black.opacity(0.55), in: .capsule)
                        .foregroundStyle(.white)
                        .padding(DS.Space.xs)
                } else {
                    Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                        .font(.system(size: 14))
                        .foregroundStyle(selected ? DS.accent : DS.textSecondary)
                        .background(Circle().fill(.black.opacity(selected ? 0 : 0.25)))
                        .padding(DS.Space.xs)
                }
            }
            .overlay(RoundedRectangle(cornerRadius: DS.Radius.s)
                .strokeBorder(selected ? DS.accent : .clear, lineWidth: 1.5))
            Text(candidate.filename).font(DS.Font.caption).lineLimit(1)
            Text("\(candidate.captureDate.formatted(date: .omitted, time: .standard)) · \(Formatters.bytes(candidate.size))")
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary).lineLimit(1)
        }
        .opacity(locked ? 0.4 : (selected ? 1 : 0.7))
        .task(id: "\(card ?? "")|\(candidate.url.path)") {
            image = thumbnails.cached(candidate.url, card: card)
            if image == nil { image = await thumbnails.thumbnail(for: candidate.url, card: card) }
        }
        .contentShape(.rect)
        .help(locked ? "Already imported. Turn off “Skip duplicates” to import again." : candidate.url.path)
    }
}

private struct ImportOptions: View {
    @Bindable var importer: ImportModel

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: DS.Space.l) {
                if let p = importer.resuming { // H2: this card's unfinished import continues
                    VStack(alignment: .leading, spacing: DS.Space.xs) {
                        Label(ImportWording.resumeNote(sessionName: p.sessionName, left: p.pending + p.failed),
                              systemImage: "arrow.clockwise.circle")
                            .font(.system(size: 12, weight: .semibold))
                        Button("Discard Unfinished Copies", role: .destructive) { Task { await importer.discardUnfinished() } }
                            .buttonStyle(.link).font(DS.Font.caption)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(DS.Space.s)
                    .background(DS.accent.opacity(0.08), in: .rect(cornerRadius: DS.Radius.m))
                }
                if importer.hasCardIssues { // listed before anything is copied, never hidden
                    CardIssuesList(scanErrors: importer.scanErrors, skipped: importer.skippedUnknown)
                }
                if importer.damagedCount > 0 {
                    Label(ImportWording.damagedNote(importer.damagedCount), systemImage: "exclamationmark.triangle")
                        .font(DS.Font.label).foregroundStyle(DS.star)
                }
                section("Destination") {
                    Picker("", selection: $importer.destination) {
                        Text("New Session").tag(ImportModel.Destination.newSession)
                        if !importer.existingSessions.isEmpty {
                            Text("Existing").tag(existingTag)
                        }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    switch importer.destination {
                    case .newSession:
                        TextField("Name", text: $importer.sessionText, prompt: Text("e.g. Fall Hike"))
                            .textFieldStyle(.roundedBorder)
                        TextField("Template", text: $importer.nameTemplate)
                            .textFieldStyle(.roundedBorder)
                            .font(DS.Font.value)
                            .help("Tokens: {date} {time} {text}")
                    case .existing:
                        Picker("Session", selection: $importer.destination) {
                            ForEach(importer.existingSessions) { s in
                                Text(s.name).tag(ImportModel.Destination.existing(s.id))
                            }
                        }
                        .labelsHidden()
                    }
                    Label(importer.destinationName, systemImage: "folder")
                        .font(DS.Font.label).foregroundStyle(DS.textSecondary).lineLimit(1)
                }
                section("Rename Files") {
                    HStack {
                        TextField("Template", text: $importer.renameTemplate)
                            .textFieldStyle(.roundedBorder)
                            .font(DS.Font.value)
                        Menu {
                            ForEach(ImportTemplates.renamePresets, id: \.self) { p in
                                Button(p) { importer.renameTemplate = p }
                            }
                        } label: { Image(systemName: "list.bullet") }
                            .menuStyle(.borderlessButton)
                            .fixedSize()
                            .help("Presets. Tokens: {original} {date} {time} {seq} {seq4} {camera} {text}")
                    }
                    VStack(alignment: .leading, spacing: DS.Space.xxs) {
                        ForEach(importer.renamePreview(), id: \.self) { Text($0) }
                    }
                    .font(DS.Font.value)
                    .foregroundStyle(DS.textSecondary)
                }
                section("Subfolder") {
                    TextField("None", text: $importer.subfolderTemplate, prompt: Text("e.g. {date}"))
                        .textFieldStyle(.roundedBorder)
                        .font(DS.Font.value)
                        .help("Folder inside the session’s Capture folder. Tokens: {date} {time} {camera} {text}")
                    if let preview = importer.subfolderPreview {
                        Label("Capture/\(preview)", systemImage: "folder")
                            .font(DS.Font.value).foregroundStyle(DS.textSecondary).lineLimit(1)
                    }
                }
                section("Backup Copy") {
                    HStack {
                        Text(importer.backupFolder?.path ?? "None")
                            .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                            .lineLimit(1).truncationMode(.middle)
                        Spacer()
                        if importer.backupFolder != nil {
                            Button { importer.backupFolder = nil } label: { Image(systemName: "xmark.circle.fill") }
                                .buttonStyle(.plain).foregroundStyle(DS.textSecondary)
                                .help("No backup copy")
                                .accessibilityLabel("Remove backup destination")
                        }
                        Button("Choose…") { chooseBackup() }
                    }
                    Text(ImportWording.backupNote(verifies: importer.verifies))
                        .font(DS.Font.caption).foregroundStyle(DS.textTertiary)
                }
                section("Options") {
                    Toggle("Skip duplicates", isOn: $importer.skipDuplicates)
                        .help("Files already in any session are not copied again")
                    Toggle("Re-read the card after copying", isOn: $importer.verifySourceReread)
                        .help("Reads every file from the card a second time and compares it. Catches a card reader that returned bad data once; takes twice as long.")
                }
            }
            .padding(DS.Space.m)
        }
        .disabled(importer.isBusy)
    }

    private var existingTag: ImportModel.Destination {
        if case .existing = importer.destination { return importer.destination }
        return .existing(importer.existingSessions[0].id)
    }

    private func section(_ title: String, @ViewBuilder _ content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Text(title.uppercased()).font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
            content()
        }
    }

    private func chooseBackup() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Use for Backup"
        if panel.runModal() == .OK { importer.backupFolder = panel.url }
    }
}

private struct ImportFooter: View {
    let importer: ImportModel
    let close: () -> Void

    var body: some View {
        HStack(spacing: DS.Space.m) {
            status
            Spacer()
            switch importer.phase {
            case .finished:
                Button("Done", action: close).keyboardShortcut(.defaultAction)
            case .importing:
                Button(importer.isStopping ? "Stopping…" : "Stop Import") { importer.cancel() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(importer.isStopping)
            case .failed where !importer.candidates.isEmpty:
                Button("Cancel", action: close).keyboardShortcut(.cancelAction)
                Button("Try Again") { importer.retry() }.keyboardShortcut(.defaultAction)
            case _ where !importer.copyEnabled:
                Button("Close", action: close).keyboardShortcut(.cancelAction)
            default:
                Button("Cancel", action: close).keyboardShortcut(.cancelAction)
                let videos = importer.selected.filter { $0.kind == .video }.count
                Button(ImportWording.importButton(photos: importer.selected.count - videos, videos: videos)) { importer.start() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!importer.canStart)
            }
        }
        .padding(DS.Space.m)
        .frame(height: 64)
    }

    @ViewBuilder private var status: some View {
        switch importer.phase {
        case .importing:
            let p = importer.progress
            VStack(alignment: .leading, spacing: DS.Space.xs) {
                ProgressView(value: p?.fraction ?? 0).frame(width: 360)
                HStack(spacing: DS.Space.m) {
                    Text("\(p?.filesDone ?? 0) of \(p?.filesTotal ?? 0) files")
                    Text("\(Formatters.bytes(p?.bytesDone ?? 0)) of \(Formatters.bytes(p?.bytesTotal ?? 0))")
                    Text(importer.throughput.map { Formatters.throughput(bytesPerSecond: $0) } ?? "–")
                    Text(importer.eta.map { "\(Formatters.duration($0)) left" } ?? "Estimating…")
                }
                .font(DS.Font.value).foregroundStyle(DS.textSecondary)
            }
        case .failed(let message) where !importer.candidates.isEmpty:
            Label(message, systemImage: "exclamationmark.triangle.fill")
                .font(DS.Font.label).foregroundStyle(DS.reject).lineLimit(2)
        case .ready where importer.copyEnabled && importer.lacksSpace:
            Label(ImportWording.notEnoughSpace(needed: importer.neededBytes, free: importer.freeBytes ?? 0),
                  systemImage: "externaldrive.badge.exclamationmark")
                .font(DS.Font.label).foregroundStyle(DS.reject).lineLimit(2)
        case .ready where importer.copyEnabled:
            Text(ImportWording.readyLine(bytes: importer.selectedBytes, destination: importer.destinationName,
                                         verifies: importer.verifies, free: importer.freeBytes))
                .font(DS.Font.label).foregroundStyle(DS.textSecondary).lineLimit(1)
        default:
            EmptyView()
        }
    }
}

private struct ImportDoneView: View {
    let summary: ImportModel.ImportSummary
    @Bindable var importer: ImportModel

    var body: some View {
        ScrollView {
            VStack(spacing: DS.Space.m) {
                Image(systemName: summary.safeToRemove ? "checkmark.seal.fill"
                    : (summary.verified ? "checkmark.circle" : "exclamationmark.triangle.fill"))
                    .font(.system(size: 44)).foregroundStyle(summary.verified ? DS.pick : DS.star)
                Text(ImportWording.doneTitle(summary)).font(DS.Font.title)
                Text(ImportWording.doneDetail(summary)).foregroundStyle(DS.textSecondary).multilineTextAlignment(.center)
                if summary.skippedDuplicates > 0 {
                    Text("\(summary.skippedDuplicates) already in the library (checked byte for byte), not copied again.")
                        .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                }
                problems
                if !summary.damaged.isEmpty {
                    Label(ImportWording.damagedNote(summary.damaged.count), systemImage: "exclamationmark.triangle")
                        .font(DS.Font.label).foregroundStyle(DS.star)
                }
                if !summary.stayOnCard.isEmpty { // exactly which files are only on the card (H1)
                    StayOnCardList(files: summary.stayOnCard)
                }
                if let gate = importer.ejectGate, gate.offersEject {
                    Button(gate.buttonTitle) { Task { await importer.ejectNow() } }
                        .buttonStyle(.borderedProminent).tint(gate.safe ? DS.accent : DS.reject)
                        .help(ImportWording.ejectHelp(canEject: true))
                        .confirmationDialog(ImportWording.ejectAnywayTitle(summary.stayOnCard.count),
                                            isPresented: $importer.confirmingEject) {
                            Button("Eject Anyway", role: .destructive) { Task { await importer.confirmEject() } }
                            Button("Keep the Card", role: .cancel) {}
                        } message: {
                            Text(ImportWording.ejectAnywayMessage(summary))
                        }
                }
                if let line = ImportWording.cardLine(summary) {
                    Label(line, systemImage: summary.ejected ? "eject.fill"
                        : (summary.safeToRemove ? "checkmark.shield" : "exclamationmark.triangle"))
                        .foregroundStyle(summary.ejectError != nil || !summary.safeToRemove ? DS.reject : DS.textSecondary)
                }
            }
            .frame(maxWidth: 560)
            .padding(DS.Space.l)
            .frame(maxWidth: .infinity)
        }
    }

    @ViewBuilder private var problems: some View {
        let failed = summary.failed + summary.registerErrors
        if !failed.isEmpty {
            VStack(alignment: .leading, spacing: DS.Space.xs) {
                Label("\(failed.count) file\(failed.count == 1 ? "" : "s") need attention", systemImage: "xmark.octagon")
                    .font(.system(size: 12, weight: .semibold)).foregroundStyle(DS.reject)
                ForEach(failed.prefix(6), id: \.self) { i in
                    Text("\(i.url.lastPathComponent): \(i.reason)").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                        .lineLimit(2)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(DS.Space.s)
            .background(DS.reject.opacity(0.08), in: .rect(cornerRadius: DS.Radius.m))
        }
    }
}

/// What stays only on the card after an import (H1): never hidden.
struct StayOnCardList: View {
    let files: [ImportIssue]

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            Label(ImportWording.stayOnCardTitle(files.count), systemImage: "sdcard")
                .font(.system(size: 12, weight: .semibold)).foregroundStyle(DS.reject)
            ForEach(Array(files.prefix(8).enumerated()), id: \.offset) { _, i in
                Text("\(i.url.lastPathComponent) — \(i.reason)")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary).lineLimit(1).truncationMode(.middle)
                    .help(i.url.path)
            }
            if files.count > 8 {
                Text("and \(files.count - 8) more").font(DS.Font.caption).foregroundStyle(DS.textTertiary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(DS.Space.s)
        .background(DS.reject.opacity(0.08), in: .rect(cornerRadius: DS.Radius.m))
        .accessibilityElement(children: .combine)
    }
}

/// Card files an import won't take or can't read (ds-import `scan_errors` /
/// `skipped_unknown`): always listed, never hidden.
struct CardIssuesList: View {
    let scanErrors: [ImportIssue]
    let skipped: [ImportIssue]

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            Label(ImportWording.issuesTitle(scanErrors: scanErrors.count, skipped: skipped.count),
                  systemImage: scanErrors.isEmpty ? "info.circle" : "exclamationmark.triangle.fill")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(scanErrors.isEmpty ? DS.textSecondary : DS.reject)
            ForEach(Array((scanErrors + skipped).prefix(8).enumerated()), id: \.offset) { _, i in
                Text("\(i.url.lastPathComponent) — \(i.reason)")
                    .font(DS.Font.caption).foregroundStyle(DS.textSecondary).lineLimit(1).truncationMode(.middle)
                    .help(i.url.path)
            }
            let more = scanErrors.count + skipped.count - 8
            if more > 0 {
                Text("and \(more) more").font(DS.Font.caption).foregroundStyle(DS.textTertiary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(DS.Space.s)
        .background(DS.star.opacity(0.08), in: .rect(cornerRadius: DS.Radius.m))
        .accessibilityElement(children: .combine)
    }
}

/// Live mode: explains why nothing will be copied, offers read-only browsing.
private struct LiveImportNotice: View {
    let importer: ImportModel
    let close: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: DS.Space.m) {
            Image(systemName: "lock.shield").font(.system(size: 20)).foregroundStyle(DS.accent)
            VStack(alignment: .leading, spacing: DS.Space.xs) {
                Text(ImportWording.liveUnavailableTitle).font(.system(size: 13, weight: .semibold))
                Text(ImportWording.liveUnavailableMessage)
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
            if importer.canBrowse {
                Button("Browse Card") {
                    Task {
                        await importer.browse()
                        close()
                    }
                }
                .buttonStyle(.borderedProminent)
                .tint(DS.accent)
                .help("Open the card read-only in the browser. Nothing is copied or changed.")
            }
        }
        .padding(DS.Space.m)
        .background(DS.accent.opacity(0.08))
    }
}

private struct ImportEmptyState: View {
    let title: String
    let message: String
    let symbol: String
    let action: (String, () -> Void)

    var body: some View {
        VStack(spacing: DS.Space.m) {
            Image(systemName: symbol).font(.system(size: 40, weight: .light)).foregroundStyle(DS.textTertiary)
            Text(title).font(.system(size: 15, weight: .semibold))
            Text(message).font(DS.Font.label).foregroundStyle(DS.textSecondary)
                .multilineTextAlignment(.center).frame(maxWidth: 360)
            Button(action.0, action: action.1)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
