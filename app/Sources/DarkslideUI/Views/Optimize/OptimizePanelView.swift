import DarkslideCore
import SwiftUI

/// The AI Optimize panel (floating over the top-right of the window, so it is there
/// whatever the toolbar shows; Esc or ⌥⌘A closes it): the summary, then one row
/// per proposed change with its explanation (single photo; the viewer previews the
/// checked rows live), or one line per photo (batch). Texts use the panel tokens, which
/// are ≥ 4.5:1 on `DS.panelBackground`.
package struct OptimizePanelView: View {
    /// The panel's width (the viewer HUD keeps clear of it).
    package static let width: CGFloat = 460
    @Bindable var panel: OptimizePanelModel
    /// The window's height: the panel never grows past it (the list scrolls).
    var maxHeight: CGFloat = .infinity
    let close: () -> Void
    @AccessibilityFocusState private var voiceOverHere: Bool

    package init(panel: OptimizePanelModel, maxHeight: CGFloat = .infinity, close: @escaping () -> Void) {
        self.panel = panel
        self.maxHeight = maxHeight
        self.close = close
    }

    package var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            header
            if !panel.summary.isEmpty, !panel.isBatch {
                Text(panel.summary)
                    .font(.system(size: 12))
                    .foregroundStyle(DS.text)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityLabel("Summary: \(panel.summary)")
            }
            if let p = panel.progressText {
                HStack(spacing: DS.Space.s) {
                    ProgressView().controlSize(.small)
                    Text(p).font(DS.Font.label).foregroundStyle(DS.textSecondary)
                }
                .accessibilityElement(children: .combine)
            }
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: DS.Space.s) {
                    if panel.isBatch { batchRows } else { singleRows }
                }
                .padding(.vertical, DS.Space.xs)
            }
            .frame(maxHeight: min(420, max(120, maxHeight - 220)))
            Divider()
            footer
        }
        .padding(DS.Space.m)
        .frame(width: Self.width)
        .background(DS.panelBackground, in: .rect(cornerRadius: DS.Radius.l))
        .overlay(RoundedRectangle(cornerRadius: DS.Radius.l).strokeBorder(DS.hairline))
        .shadow(color: .black.opacity(0.35), radius: 16, y: 6)
        .frame(maxHeight: maxHeight, alignment: .top)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("AI Optimize")
        .onAppear { voiceOverHere = true }
    }

    private var header: some View {
        HStack(spacing: DS.Space.s) {
            Image(systemName: "sparkles").foregroundStyle(DS.accent).accessibilityHidden(true)
            Text(panel.isBatch ? "AI Optimize \(panel.items.count) Photos" : "AI Optimize")
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(DS.text)
            Spacer()
            if !panel.isBatch, let name = panel.items.first?.filename {
                Text(name).font(DS.Font.label).foregroundStyle(DS.textSecondary).lineLimit(1)
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
        .accessibilityFocused($voiceOverHere) // VoiceOver starts in the panel when it opens
    }

    @ViewBuilder private var singleRows: some View {
        if let item = panel.items.first {
            switch item.state {
            case .analyzing:
                EmptyView()
            case .failed(let why):
                Text("Couldn't analyze this photo: \(why)")
                    .font(DS.Font.label).foregroundStyle(DS.text)
            case .ready(let r):
                if r.changes.isEmpty {
                    Text("Nothing to change: this photo already looks balanced.")
                        .font(DS.Font.label).foregroundStyle(DS.text)
                }
                ForEach(r.changes) { c in ChangeRow(change: c, isOn: panel.isSelected(c)) { panel.toggle(c.id) } }
                if !r.kept.isEmpty {
                    Text("Left alone")
                        .font(DS.Font.panelTitle).foregroundStyle(DS.textSecondary)
                        .padding(.top, DS.Space.xs)
                        .accessibilityAddTraits(.isHeader)
                    ForEach(Array(r.kept.enumerated()), id: \.offset) { _, k in
                        Text(k.explanation)
                            .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        }
    }

    private var batchRows: some View {
        ForEach(panel.items) { item in
            HStack(alignment: .top, spacing: DS.Space.s) {
                Toggle("", isOn: Binding(get: { item.include }, set: { panel.setInclude(item.id, $0) }))
                    .toggleStyle(.checkbox)
                    .labelsHidden()
                    .disabled(item.report?.changes.isEmpty ?? true)
                    .accessibilityLabel("Apply to \(item.filename)")
                VStack(alignment: .leading, spacing: 2) {
                    Text(item.filename).font(DS.Font.label.weight(.semibold)).foregroundStyle(DS.text)
                    Text(Self.status(item))
                        .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .accessibilityElement(children: .combine)
            .accessibilityValue(Self.status(item))
        }
    }

    static func status(_ item: OptimizePanelModel.Item) -> String {
        switch item.state {
        case .analyzing: "Analyzing…"
        case .failed(let why): "Couldn't analyze: \(why)"
        case .ready(let r): r.compactSummary
        }
    }

    private var footer: some View {
        HStack {
            if !panel.isBatch {
                Toggle("Show before", isOn: $panel.showBefore)
                    .toggleStyle(.checkbox)
                    .font(DS.Font.label)
                    .disabled(panel.report == nil)
                    .help("Show the photo without the checked changes")
            }
            Spacer()
            Button("Cancel", role: .cancel, action: close).keyboardShortcut(.cancelAction)
            Button(panel.applyTitle) { panel.apply() }
                .keyboardShortcut(.defaultAction)
                .disabled(!panel.canApply)
        }
    }
}

/// One proposed change: checkbox, icon, field, old → new, and why.
private struct ChangeRow: View {
    let change: OptimizeChange
    let isOn: Bool
    let toggle: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: DS.Space.s) {
            Toggle("", isOn: Binding(get: { isOn }, set: { _ in toggle() }))
                .toggleStyle(.checkbox)
                .labelsHidden()
            Image(systemName: change.field.symbol)
                .frame(width: 18)
                .foregroundStyle(DS.accent)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: DS.Space.xs) {
                    Text(change.label).font(DS.Font.label.weight(.semibold)).foregroundStyle(DS.text)
                    Text("\(change.from) → \(change.to)").font(DS.Font.value).foregroundStyle(DS.text)
                }
                Text(change.explanation)
                    .font(DS.Font.label)
                    .foregroundStyle(DS.textSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .contentShape(.rect)
        .onTapGesture(perform: toggle)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(change.label), \(change.from) to \(change.to)")
        .accessibilityValue(isOn ? "checked" : "unchecked")
        .accessibilityHint(change.explanation)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction { toggle() }
    }
}

/// "Why these settings?": the last applied AI Optimize report of the photo.
struct OptimizeWhyView: View {
    let report: OptimizeReport
    /// The applied changes the photo still has.
    let changes: [OptimizeChange]
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack {
                Image(systemName: "info.circle").foregroundStyle(DS.accent).accessibilityHidden(true)
                Text("Why these settings?").font(.system(size: 14, weight: .semibold)).foregroundStyle(DS.text)
                    .accessibilityAddTraits(.isHeader)
                Spacer()
            }
            Text(OptimizeWhy.intro(report, inEffect: changes.count)).font(.system(size: 12)).foregroundStyle(DS.text)
                .fixedSize(horizontal: false, vertical: true)
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: DS.Space.s) {
                    ForEach(changes) { c in
                        HStack(alignment: .top, spacing: DS.Space.s) {
                            Image(systemName: c.field.symbol).frame(width: 18).foregroundStyle(DS.accent)
                                .accessibilityHidden(true)
                            Text(c.explanation).font(DS.Font.label).foregroundStyle(DS.text)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        .accessibilityElement(children: .combine)
                    }
                    ForEach(Array(report.kept.enumerated()), id: \.offset) { _, k in
                        Text(k.explanation).font(DS.Font.label).foregroundStyle(DS.textSecondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            .frame(maxHeight: 360)
            HStack {
                Text(OptimizeWhy.caveat).font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                Spacer()
                Button("Done", action: close).keyboardShortcut(.defaultAction)
            }
        }
        .padding(DS.Space.m)
        .frame(width: 420)
        .background(DS.panelBackground)
    }
}
