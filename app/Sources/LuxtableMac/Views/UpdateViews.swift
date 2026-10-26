import LuxtableCore
import LuxtableUI
import SwiftUI

/// Settings ▸ Updates: automatic checks, the beta channel, Check Now.
struct UpdateSettingsView: View {
    @Bindable var updater: Updater

    var body: some View {
        Form {
            Section {
                LabeledContent("Version", value: updater.isAvailable ? updater.currentVersion : "—")
                Toggle("Check for updates automatically", isOn: $updater.automatic)
                Toggle("Include beta versions", isOn: $updater.includeBeta)
                Text("Betas are early builds of the next version, for trying new features before they’re released. They may have bugs.")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Section {
                HStack {
                    Text(UpdateText.status(updater))
                        .foregroundStyle(DS.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                    if case .available = updater.state {
                        Button("View Update…") { updater.showSheet() }
                    }
                    Button("Check Now") { updater.checkNow() }
                        .disabled(!updater.isAvailable || updater.isBusy)
                }
                Text("Luxtable asks GitHub for the list of its releases; the request carries only the app’s version. An update is checked against the SHA-256 GitHub publishes and its signature before it’s installed, and the version it replaces is kept in Luxtable’s Application Support folder (“Previous Version”).")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .formStyle(.grouped)
    }
}

/// The update sheet: what's new, then download, verify and restart.
struct UpdateSheet: View {
    @Bindable var updater: Updater

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            HStack(spacing: DS.Space.s) {
                Text(UpdateText.title(updater.state)).font(.headline)
                if let r = release, r.isPrerelease {
                    Text("Beta").font(DS.Font.label.weight(.semibold))
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(DS.accent.opacity(0.2), in: .capsule)
                }
            }
            let detail = UpdateText.detail(updater.state, current: updater.currentVersion, beta: updater.includeBeta)
            if !detail.isEmpty, !isDownloading {
                Text(detail).foregroundStyle(DS.textSecondary).fixedSize(horizontal: false, vertical: true)
            }
            switch updater.state {
            case .checking, .idle:
                ProgressView().progressViewStyle(.linear)
            case .downloading(_, let f):
                ProgressView(value: f)
                Text(detail).font(DS.Font.label).foregroundStyle(DS.textSecondary)
            case .verifying:
                ProgressView().progressViewStyle(.linear)
            default:
                EmptyView()
            }
            if let r = release, case .available = updater.state, !r.notes.isEmpty {
                ReleaseNotesList(title: "What’s new", notes: r.notes)
                if r.version.description == updater.skipped {
                    Text("You skipped this version.").font(DS.Font.label).foregroundStyle(DS.textSecondary)
                }
            }
            HStack {
                buttons
            }
        }
        .padding(DS.Space.l)
        .frame(width: 480)
    }

    private var release: UpdateRelease? {
        switch updater.state {
        case .available(let r), .downloading(let r, _), .verifying(let r), .ready(let r), .manualInstall(let r, _): r
        default: nil
        }
    }

    private var isDownloading: Bool {
        if case .downloading = updater.state { return true }
        return false
    }

    @ViewBuilder
    private var buttons: some View {
        switch updater.state {
        case .available:
            Button("Skip This Version") { updater.skip() }
            Spacer()
            Button("Later") { updater.later() }.keyboardShortcut(.cancelAction)
            Button("Download") { Task { await updater.download(restart: false) } }
                .help("Download now; it installs when you quit Luxtable")
            Button("Download & Restart") { Task { await updater.download(restart: true) } }
                .keyboardShortcut(.defaultAction)
        case .downloading, .verifying:
            Spacer()
            Button("Cancel") { updater.cancelDownload() }.keyboardShortcut(.cancelAction)
        case .ready:
            Spacer()
            Button("Later") { updater.later() }.keyboardShortcut(.cancelAction)
            Button("Restart Now") { updater.restart() }.keyboardShortcut(.defaultAction)
        case .checking, .idle:
            Spacer()
            Button("Cancel") { updater.later() }.keyboardShortcut(.cancelAction)
        case .upToDate, .failed, .manualInstall:
            Spacer()
            Button("OK") { updater.later() }.keyboardShortcut(.defaultAction)
        }
    }
}

/// "Updated to X": shown once after an update.
struct WhatsNewSheet: View {
    let whatsNew: Updater.WhatsNew
    let done: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.m) {
            Text("Updated to Luxtable \(whatsNew.version)").font(.headline)
            if whatsNew.notes.isEmpty {
                Text("You’re back where you left off.").foregroundStyle(DS.textSecondary)
            } else {
                ReleaseNotesList(title: "What’s new", notes: whatsNew.notes)
            }
            HStack {
                Spacer()
                Button("Continue", action: done).keyboardShortcut(.defaultAction)
            }
        }
        .padding(DS.Space.l)
        .frame(width: 480)
    }
}

/// The short release notes as bullets, scrolling when long.
struct ReleaseNotesList: View {
    let title: String
    let notes: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            Text(title).font(.subheadline.weight(.semibold))
            ScrollView {
                VStack(alignment: .leading, spacing: DS.Space.s) {
                    ForEach(Array(notes.enumerated()), id: \.offset) { _, note in
                        HStack(alignment: .firstTextBaseline, spacing: DS.Space.s) {
                            Text("•")
                            Text(note).fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 240)
        }
    }
}

/// The main window's notice after an automatic check found an update: never a modal.
struct UpdateNotice: View {
    @Bindable var updater: Updater

    var body: some View {
        if let r = updater.notice {
            HStack(spacing: DS.Space.s) {
                Label("Luxtable \(r.version.description)\(r.isPrerelease ? " beta" : "") is available", systemImage: "arrow.down.circle")
                Button("View") { updater.showSheet() }.buttonStyle(.borderless).foregroundStyle(DS.accent)
                Button { updater.later() } label: { Image(systemName: "xmark") }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Dismiss")
            }
            .font(.callout.weight(.medium))
            .padding(.horizontal, DS.Space.m)
            .padding(.vertical, DS.Space.s)
            .glassEffect(.regular, in: .capsule)
            .padding(DS.Space.m)
            .transition(.move(edge: .bottom).combined(with: .opacity))
        }
    }
}

/// The update sheet, "Updated to X" and the notice, on the main window.
struct UpdatePresentation: ViewModifier {
    @Bindable var updater: Updater

    func body(content: Content) -> some View {
        content
            .overlay(alignment: .bottomTrailing) { UpdateNotice(updater: updater) }
            .animation(.snappy(duration: 0.2), value: updater.notice)
            .background {
                Color.clear
                    .sheet(isPresented: $updater.sheetShown) { UpdateSheet(updater: updater) }
            }
            .background {
                Color.clear
                    .sheet(item: $updater.whatsNew) { w in WhatsNewSheet(whatsNew: w) { updater.whatsNew = nil } }
            }
    }
}
