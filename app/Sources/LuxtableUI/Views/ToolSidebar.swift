import LuxtableCore
import SwiftUI

/// Tool tabs across the top of the tools pane. Click an icon to
/// show its tools; drag icons to reorder; drop a tool on an icon to add it to that
/// tab; hover shows the name. Tabs that don't fit go in the » menu; + adds or
/// removes tools in the selected tab.
struct ToolTabStrip: View {
    @Bindable var model: AppModel
    var layout = ToolLayout.shared
    /// The strip's inner width (everything fits until it is measured).
    @State private var width = Double.infinity
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let selected = ToolWorkspaceCommands.selectedTab(model: model, layout: layout)
        let split = ToolStrip.split(layout.order, selected: selected, width: width)
        HStack(spacing: ToolStrip.spacing) {
            ForEach(split.visible) { tab in
                tabButton(tab, selected: selected == tab)
                if tab == .favorites { Divider().frame(height: 18).padding(.horizontal, 4) }
            }
            if !split.overflow.isEmpty {
                let changed = model.aiChangedPanels
                let aiChanged = ToolStrip.hasAIChanges(split.overflow, panels: layout.panels(for:), changed: changed)
                let more = ToolStripControl.moreTabs(split.overflow.count)
                Menu {
                    ForEach(split.overflow) { tab in
                        let changes = AIHighlightStyle.tabHasChanges(layout.panels(for: tab), changed: changed)
                        Button { model.toolTab = tab } label: {
                            Label(layout.workspace.title(of: tab, names: model.settings.names) + (changes ? " •" : ""), systemImage: layout.workspace.symbol(of: tab))
                        }
                        .accessibilityLabel(layout.workspace.title(of: tab, names: model.settings.names) + (changes ? AIHighlightStyle.containerSuffix : ""))
                    }
                    Divider()
                    // The tab context menu for tabs not in the strip.
                    Section("Arrange") {
                        ForEach(split.overflow) { tab in
                            Menu(layout.workspace.title(of: tab, names: model.settings.names)) { TabContextItems(tab: tab, model: model, layout: layout) }
                        }
                    }
                } label: {
                    stripIcon("chevron.right.2")
                        .overlay(alignment: .topTrailing) { if aiChanged { AIBadge(size: 8).offset(x: 1, y: 1) } }
                }
                    .menuStyle(.button).buttonStyle(.plain).menuIndicator(.hidden)
                    .help(more.label)
                    .accessibilityLabel(more.label + (aiChanged ? AIHighlightStyle.containerSuffix : ""))
                    .accessibilityHint(more.hint ?? "")
            }
            Spacer(minLength: 0)
            let add = ToolStripControl.addTool(tabTitle: layout.workspace.title(of: selected, names: model.settings.names))
            Menu { AddToolItems(tab: selected, model: model, layout: layout) } label: { stripIcon("plus") }
                .menuStyle(.button).buttonStyle(.plain).menuIndicator(.hidden)
                .help("Add or remove tools in \(layout.workspace.title(of: selected, names: model.settings.names))")
                .accessibilityLabel(add.label)
                .accessibilityHint(add.hint ?? "")
        }
        .padding(.horizontal, DS.Space.s)
        // minWidth 0: the pane's width, not the icons', is what gets measured.
        .frame(minWidth: 0, maxWidth: .infinity)
        .frame(height: 36)
        .background(DS.chrome)
        .onGeometryChange(for: Double.self) { Double($0.size.width - 2 * DS.Space.s) } action: { width = $0 }
        .contextMenu {
            Button("Customize Tools…") { openWindow(id: ToolCustomizeView.windowID) }
            Button("Reset Tab Order") { layout.resetOrder() }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Tool tabs")
    }

    private func stripIcon(_ symbol: String) -> some View {
        Image(systemName: symbol)
            .font(.system(size: 12, weight: .semibold))
            .foregroundStyle(DS.textSecondary)
            .frame(width: ToolStrip.buttonWidth - ToolStrip.spacing, height: ToolStrip.iconWidth)
            .contentShape(.rect)
    }

    private func tabButton(_ tab: ToolTab, selected: Bool) -> some View {
        StripButton(tab: tab, title: layout.workspace.title(of: tab, names: model.settings.names), symbol: layout.workspace.symbol(of: tab),
                    selected: selected,
                    aiChanged: AIHighlightStyle.tabHasChanges(layout.panels(for: tab), changed: model.aiChangedPanels)) {
            model.toolTab = tab
        }
            .contextMenu { TabContextItems(tab: tab, model: model, layout: layout) }
            .accessibilityAction(named: "Move Left") { layout.move(tab, by: -1) }
            .accessibilityAction(named: "Move Right") { layout.move(tab, by: 1) }
            .draggable(ToolDragItem.tab(tab))
            .dropDestination(for: ToolDragItem.self) { items, _ in
                switch items.first {
                case .panel(let panel): return ToolWorkspaceCommands.drop(panel, onTab: tab, model: model, layout: layout)
                case .tab(let dragged):
                    withAnimation(.snappy(duration: 0.2)) { layout.move(dragged, before: tab) }
                    return true
                case nil: return false
                }
            }
    }
}

/// A tab's context menu (strip icons, and the » menu's Arrange items).
private struct TabContextItems: View {
    let tab: ToolTab
    @Bindable var model: AppModel
    var layout = ToolLayout.shared
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        // N17: keyboard- and VoiceOver-reachable reordering (also View ▸ Move Tool Tab).
        Button("Move Left") { withAnimation(.snappy(duration: 0.2)) { layout.move(tab, by: -1) } }
            .disabled(!layout.canMove(tab, by: -1))
        Button("Move Right") { withAnimation(.snappy(duration: 0.2)) { layout.move(tab, by: 1) } }
            .disabled(!layout.canMove(tab, by: 1))
        Divider()
        AddToolMenu(tab: tab, model: model, layout: layout)
        Button("New Tab") {
            ToolWorkspaceCommands.newTab(title: "New Tab", model: model, layout: layout)
            layout.customizeFocus = .tabName
            openWindow(id: ToolCustomizeView.windowID)
        }
        Button("Rename or Change Icon…") {
            model.toolTab = tab
            layout.customizeFocus = .tabName
            openWindow(id: ToolCustomizeView.windowID)
        }
        Button("Remove Tab") { ToolWorkspaceCommands.removeTab(tab, model: model, layout: layout) }
            .disabled(!layout.workspace.canRemoveTab(tab))
        Divider()
        Button("Customize Tools…") { openWindow(id: ToolCustomizeView.windowID) }
        Button("Reset Tab Order") { layout.resetOrder() }
    }
}

private struct StripButton: View {
    let tab: ToolTab
    let title: String
    let symbol: String
    let selected: Bool
    /// A panel in this tab has a row an AI edit changed.
    var aiChanged = false
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 14, weight: selected ? .semibold : .regular))
                .frame(width: ToolStrip.iconWidth, height: ToolStrip.iconWidth)
                .foregroundStyle(selected ? DS.accent : DS.textSecondary)
                .background(selected ? DS.accent.opacity(0.16) : (hovering ? Color.primary.opacity(0.07) : .clear),
                            in: .rect(cornerRadius: DS.Radius.m))
                .contentShape(.rect)
                .overlay(alignment: .topTrailing) { if aiChanged { AIBadge(size: 8).offset(x: 2, y: -1) } }
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        // Name flag below the icon, drawn rightward so the first tab's isn't cut off at the
        // window edge (over the histogram and viewer: the strip and sidebar have zIndex 1).
        .overlay(alignment: .bottomLeading) {
            if hovering {
                Text(title)
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(DS.text)
                    .fixedSize()
                    .padding(.horizontal, DS.Space.s)
                    .padding(.vertical, DS.Space.xs)
                    .background(DS.chrome, in: .capsule)
                    .overlay(Capsule().strokeBorder(DS.hairline))
                    .shadow(color: .black.opacity(0.3), radius: 4, y: 1)
                    .offset(y: 30)
                    .allowsHitTesting(false)
                    .transition(.opacity)
            }
        }
        .animation(.easeOut(duration: 0.12), value: hovering)
        .help(tab == .favorites ? "Favorites: right-click any panel header to add it here" : title)
        .accessibilityLabel(ToolStripControl.tab(tab, title: title, selected: selected).label + (aiChanged ? AIHighlightStyle.containerSuffix : ""))
        .accessibilityHint(ToolStripControl.tab(tab, title: title, selected: selected).hint ?? "")
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// The tool tabs on top, the histogram, then the selected tab's collapsible panels. The edge
/// facing the viewer drags to resize (260…480 pt, remembered); in the Lightroom Classic
/// layout Copy… / Paste sit at the foot.
package struct ToolSidebar: View {
    @Bindable var model: AppModel
    var side: LayoutPreset.Side = .left
    var layout = ToolLayout.shared
    @AppStorage(PanelWidth.storageKey) private var width = PanelWidth.default
    @State private var dragStartWidth: Double?

    package init(model: AppModel, side: LayoutPreset.Side = .left, layout: ToolLayout = .shared) {
        self.model = model
        self.side = side
        self.layout = layout
    }

    private var clipping: some View {
        Toggle("Clipping", isOn: Binding(get: { model.viewer.clippingWarning }, set: { model.viewer.clippingWarning = $0 }))
            .help("Show clipped highlights (red) and shadows (blue) on the image")
            .fixedSize()
    }

    private var focusPeaking: some View {
        Toggle("Focus peaking", isOn: Binding(get: { model.viewer.focusPeaking }, set: { model.viewer.focusPeaking = $0 }))
            .help("Highlight in-focus edges on the image")
            .fixedSize()
    }

    private var softProof: some View {
        let proof = model.softProof
        return Toggle("Soft proof", isOn: Binding(get: { proof.enabled }, set: { proof.setEnabled($0) }))
            .help("Show the photo as it will look in the chosen profile (⇧S)")
            .fixedSize()
    }

    package var body: some View {
        let tab = ToolWorkspaceCommands.selectedTab(model: model, layout: layout)
        VStack(spacing: 0) {
            ToolTabStrip(model: model, layout: layout)
                .zIndex(1)
            Divider()
            VStack(spacing: DS.Space.xs) {
                HistogramView(histogram: model.viewer.histogram,
                              title: PanelChrome.histogramTitle(rendersEdits: !(model.renderer is EmbeddedPreviewRenderer)))
                HistogramFooter(histogram: model.viewer.histogram, readout: model.viewer.cursorReadout)
                // One line when the panel is wide enough, else two centred lines.
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: DS.Space.m) { clipping; focusPeaking; softProof }
                    VStack(spacing: DS.Space.xs) {
                        HStack(spacing: DS.Space.m) { clipping; focusPeaking }
                        softProof
                    }
                }
                .frame(maxWidth: .infinity)
                .toggleStyle(.checkbox)
                .font(DS.Font.caption)
                .foregroundStyle(DS.textSecondary)
                if model.softProof.enabled { SoftProofControls(proof: model.softProof) }
            }
            .padding(DS.Space.s)
            Divider()
            HStack {
                Text(layout.workspace.title(of: tab, names: model.settings.names))
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(DS.text)
                Spacer()
                WhyButton(model: model)
                if model.editAllSelected, model.selection.count > 1 {
                    Text(PanelChrome.photoCount(model.selection.count))
                        .font(DS.Font.caption)
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(DS.accent.opacity(0.2), in: .capsule)
                        .help("Edit All Selected is on: adjustments apply to every selected photo")
                }
            }
            .padding(.horizontal, DS.Space.m)
            .padding(.vertical, DS.Space.s)
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 0) {
                        // An integration that is off (Settings ▸ Plugins) shows no panel; its place is kept.
                        let panels = layout.panels(for: tab).filter(model.features.shows)
                        if panels.isEmpty {
                            let add = ToolStripControl.addTool(tabTitle: layout.workspace.title(of: tab, names: model.settings.names))
                            VStack(spacing: DS.Space.m) {
                                Text(tab == .favorites
                                     ? "Right-click any panel header and choose “Add to Favorites” to collect your most-used tools here."
                                     : "No tools in this tab.")
                                    .font(DS.Font.caption)
                                    .foregroundStyle(DS.textSecondary)
                                    .multilineTextAlignment(.center)
                                    .fixedSize(horizontal: false, vertical: true)
                                Menu("Add Tool", systemImage: "plus") { AddToolItems(tab: tab, model: model, layout: layout) }
                                    .fixedSize()
                                    .accessibilityLabel(add.label)
                                    .accessibilityHint(add.hint ?? "")
                            }
                            .frame(maxWidth: .infinity)
                            .padding(DS.Space.l)
                        }
                        ForEach(panels) { panel in
                            ToolPanelSection(panel: panel, tab: tab, model: model, layout: layout)
                                .id(panel)
                            Divider()
                        }
                    }
                    .contextMenu { AddToolMenu(tab: tab, model: model, layout: layout) }
                    .dropDestination(for: ToolDragItem.self) { items, _ in
                        guard let panel = items.first?.panel else { return false }
                        ToolWorkspaceCommands.add(panel, to: tab, model: model, layout: layout)
                        return true
                    }
                }
                .scrollIndicators(.automatic)
                // `initial`: a reveal can bring the panels back, creating this view.
                .onChange(of: layout.revealRequest, initial: true) { _, request in
                    guard let request else { return }
                    // After the tab switch has laid out its panels.
                    Task { @MainActor in
                        withAnimation(.snappy(duration: 0.25)) { proxy.scrollTo(request.panel, anchor: .top) }
                        layout.finishReveal(request)
                    }
                }
            }
            if model.settings.layoutPreset.copyApply == .panelFoot {
                Divider()
                CopyApplyButtons(model: model, names: model.settings.names, centered: true)
                    .padding(DS.Space.s)
            }
        }
        .frame(width: PanelWidth.clamp(width))
        .background(DS.panelBackground)
        .overlay(alignment: side == .left ? .trailing : .leading) { resizeHandle }
    }

    private var resizeHandle: some View {
        Color.clear
            .frame(width: 6)
            .contentShape(.rect)
            .pointerStyle(.columnResize)
            .gesture(DragGesture(minimumDistance: 1, coordinateSpace: .global)
                .onChanged { g in
                    let start = dragStartWidth ?? PanelWidth.clamp(width)
                    if dragStartWidth == nil { dragStartWidth = start }
                    // Dragging toward the viewer widens: rightward on the left, leftward on the right.
                    let dx = Double(g.translation.width) * (side == .left ? 1 : -1)
                    width = PanelWidth.dragged(start: start, dx: dx)
                }
                .onEnded { _ in dragStartWidth = nil })
            .onTapGesture(count: 2) { width = PanelWidth.default }
            .help("Drag to resize the tool panel; double-click to reset")
    }
}

/// A collapsible panel: header (disclosure, title, copy/apply, reset) + content.
struct ToolPanelSection: View {
    let panel: ToolPanelID
    /// The tab this stack belongs to (the same tool may be in several).
    let tab: ToolTab
    @Bindable var model: AppModel
    var layout = ToolLayout.shared
    @State private var hoverHeader = false
    @Environment(\.openWindow) private var openWindow

    private var collapsed: Bool { model.collapsedPanels.contains(panel) }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: DS.Space.s) {
                // N17: the disclosure is a real button (keyboard + VoiceOver), not a tap gesture.
                Button(action: toggleCollapsed) {
                    HStack(spacing: DS.Space.s) {
                        Image(systemName: "chevron.right")
                            .font(.system(size: 9, weight: .bold))
                            .rotationEffect(.degrees(collapsed ? 0 : 90))
                            .foregroundStyle(DS.textTertiary)
                            .frame(width: 10)
                        Text(model.settings.names.title(of: panel).uppercased())
                            .font(DS.Font.panelTitle)
                            .tracking(0.6)
                            .foregroundStyle(DS.textSecondary)
                    }
                    .contentShape(.rect)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(headerControls[0].label
                                    + (model.aiChangedPanels.contains(panel) ? AIHighlightStyle.containerSuffix : ""))
                .accessibilityHint(headerControls[0].hint ?? "")
                // The stack's arranging actions, on the element VoiceOver lands on.
                .accessibilityAction(named: arrange[0].label) { layout.edit { $0.move(panel, in: tab, by: -1) } }
                .accessibilityAction(named: arrange[1].label) { layout.edit { $0.move(panel, in: tab, by: 1) } }
                .accessibilityAction(named: arrange[2].label) { layout.edit { $0.remove(panel, from: tab) } }
                if layout.isFavorite(panel), tab != .favorites {
                    Image(systemName: "star.fill").font(.system(size: 8)).foregroundStyle(DS.star)
                        .help("In Favorites")
                }
                if model.aiChangedPanels.contains(panel) {
                    Button { model.aiChangesShown = true } label: { AIBadge() }
                        .buttonStyle(.plain)
                        .help(AIHighlightStyle.panelBadgeHelp)
                        .accessibilityLabel(AIChangeText.menuTitle)
                }
                Spacer()
                if let group = panel.group {
                    let edited = model.focusedID.map { group.differs(model.edit(for: $0), EditState()) } ?? false
                    if edited {
                        Circle().fill(DS.accent).frame(width: 5, height: 5)
                            .help("Adjusted")
                    }
                    Group {
                        headerButton("doc.on.doc", help: "Copy only \(group.title)", label: headerControls[2].label) { model.copyGroup(group) }
                            .disabled(model.focusedID == nil)
                        headerButton("doc.on.clipboard", help: "Apply only \(group.title) from the clipboard to the selection",
                                     label: headerControls[3].label) {
                            model.applyGroup(group)
                        }
                        .disabled(model.clipboard == nil)
                        headerButton("arrow.counterclockwise", help: "Reset \(group.title)", label: headerControls[4].label) {
                            model.resetGroup(group)
                        }
                            .disabled(!edited)
                    }
                    .opacity(PanelChrome.headerButtonOpacity(hovering: hoverHeader))
                }
                let options = ToolStripControl.panelMenu(panel)
                Menu { PanelArrangeItems(panel: panel, tab: tab, model: model, layout: layout) } label: {
                    Image(systemName: "ellipsis").font(.system(size: 11)).frame(height: 20).contentShape(.rect)
                }
                .menuStyle(.button).buttonStyle(.plain).menuIndicator(.hidden).fixedSize()
                .foregroundStyle(DS.textSecondary)
                .opacity(PanelChrome.headerButtonOpacity(hovering: hoverHeader))
                .help(options.hint ?? options.label)
                .accessibilityLabel(options.label)
                .accessibilityHint(options.hint ?? "")
            }
            .padding(.horizontal, DS.Space.m)
            .frame(height: 30)
            .contentShape(.rect)
            .onTapGesture(perform: toggleCollapsed)
            .onHover { hoverHeader = $0 }
            // Drag the header to reorder within the tab, onto a tab icon, or into Customize Tools.
            .draggable(ToolDragItem.panel(panel)) {
                Text(model.settings.names.title(of: panel)).font(DS.Font.panelTitle).padding(DS.Space.s).background(DS.chrome, in: .rect(cornerRadius: DS.Radius.s))
            }
            .contextMenu {
                Button(headerControls[1].label) {
                    layout.toggleFavorite(panel)
                }
                Divider()
                PanelArrangeItems(panel: panel, tab: tab, model: model, layout: layout)
                Button("Customize Tools…") { openWindow(id: ToolCustomizeView.windowID) }
                if let group = panel.group {
                    Divider()
                    Button("Copy \(group.title)") { model.copyGroup(group) }
                        .disabled(model.focusedID == nil)
                    Button("Apply \(group.title) to Selection") { model.applyGroup(group) }
                        .disabled(model.clipboard == nil)
                    Button("Reset \(group.title)") { model.resetGroup(group) }
                }
                if model.canClearAIHighlights {
                    Divider()
                    Button(AIChangeText.menuTitle) { model.aiChangesShown = true }
                    Button(AIHighlightStyle.showTitle) { ToolWorkspaceCommands.revealAIChanges(model: model, layout: layout) }
                    Button(AIHighlightStyle.clearTitle) { model.clearAIHighlights() }
                }
            }

            if !collapsed {
                ToolPanelContent(panel: panel, model: model)
                    .padding(.horizontal, DS.Space.m)
                    .padding(.bottom, DS.Space.m)
                    .transition(.opacity)
            }
        }
        // A tool dropped here lands above this one (moved if it is already in the tab).
        .dropDestination(for: ToolDragItem.self) { items, _ in
            guard let dragged = items.first?.panel else { return false }
            withAnimation(.snappy(duration: 0.2)) {
                ToolWorkspaceCommands.add(dragged, to: tab, before: panel, model: model, layout: layout)
            }
            return true
        }
    }

    private var arrange: [AccessibleControl] { ToolStripControl.arrange(panel, tabTitle: layout.workspace.title(of: tab, names: model.settings.names)) }

    private func toggleCollapsed() {
        withAnimation(.snappy(duration: 0.2)) {
            if collapsed { model.collapsedPanels.remove(panel) } else { model.collapsedPanels.insert(panel) }
        }
    }

    private var headerControls: [AccessibleControl] {
        ToolStripControl.panelHeader(panel, collapsed: collapsed, favorite: layout.isFavorite(panel))
    }

    private func headerButton(_ symbol: String, help: String, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol).font(.system(size: 11))
        }
        .buttonStyle(.plain)
        .foregroundStyle(DS.textSecondary)
        .help(help)
        .accessibilityLabel(label)
    }
}

/// Arranging a tool in its tab: the header's context menu and its ⋯ menu.
private struct PanelArrangeItems: View {
    let panel: ToolPanelID
    let tab: ToolTab
    @Bindable var model: AppModel
    var layout = ToolLayout.shared

    var body: some View {
        let arrange = ToolStripControl.arrange(panel, tabTitle: layout.workspace.title(of: tab, names: model.settings.names))
        Button(arrange[2].label) { withAnimation(.snappy(duration: 0.2)) { layout.edit { $0.remove(panel, from: tab) } } }
        Divider()
        Button(arrange[0].label) { withAnimation(.snappy(duration: 0.2)) { layout.edit { $0.move(panel, in: tab, by: -1) } } }
            .disabled(!layout.workspace.canMove(panel, in: tab, by: -1))
        Button(arrange[1].label) { withAnimation(.snappy(duration: 0.2)) { layout.edit { $0.move(panel, in: tab, by: 1) } } }
            .disabled(!layout.workspace.canMove(panel, in: tab, by: 1))
        Menu("Move to") {
            let titles = layout.workspace.menuTitles
            ForEach(layout.order.filter { $0 != tab }) { other in
                Button(titles[other] ?? other.title) {
                    ToolWorkspaceCommands.move(panel, from: tab, to: other, model: model, layout: layout)
                }
            }
        }
        AddToolMenu(tab: tab, after: panel, model: model, layout: layout)
    }
}

/// "Add Tool ▸" with every tool grouped by its default tab. Those already in
/// `tab` are checked, and choosing one removes it (except `after`, the panel whose
/// menu this is); any other goes after `after`, else at the end.
struct AddToolMenu: View {
    let tab: ToolTab
    var after: ToolPanelID?
    @Bindable var model: AppModel
    var layout = ToolLayout.shared

    var body: some View {
        Menu("Add Tool") { AddToolItems(tab: tab, after: after, model: model, layout: layout) }
    }
}

struct AddToolItems: View {
    let tab: ToolTab
    var after: ToolPanelID?
    @Bindable var model: AppModel
    var layout = ToolLayout.shared

    var body: some View {
        let inTab = layout.panels(for: tab)
        ForEach(ToolWorkspaceCommands.menuSections, id: \.tab) { section in
            Section(section.tab.title) {
                ForEach(section.panels.filter(model.features.shows)) { p in
                    Toggle(p.title, isOn: Binding(get: { inTab.contains(p) }, set: { _ in
                        ToolWorkspaceCommands.toggle(p, in: tab, after: after, model: model, layout: layout)
                    }))
                    .disabled(p == after) // a panel's own menu: its Remove item does that
                }
            }
        }
    }
}

/// "Why these settings?": shown when the focused photo has an applied AI Optimize report.
private struct WhyButton: View {
    @Bindable var model: AppModel

    var body: some View {
        Group {
            if let f = model.focusedID, model.optimizeReports[f] != nil {
                Button { model.showWhy() } label: { Image(systemName: "info.circle") }
                    .buttonStyle(.plain)
                    .foregroundStyle(DS.accent)
                    .help(OptimizeWhy.buttonHelp)
                    .accessibilityLabel(OptimizeWhy.buttonLabel)
                    .popover(isPresented: Binding(get: { model.whyReportPhoto != nil },
                                                  set: { if !$0 { model.whyReportPhoto = nil } }),
                             arrowEdge: .leading) {
                        if let r = model.whyReport {
                            OptimizeWhyView(report: r, changes: model.whyChanges) { model.whyReportPhoto = nil }
                        }
                    }
            }
        }
        .task(id: model.focusedID) {
            if let f = model.focusedID { _ = await model.loadOptimizeReport(for: f) }
        }
    }
}
