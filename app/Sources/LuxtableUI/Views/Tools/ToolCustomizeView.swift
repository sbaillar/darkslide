import LuxtableCore
import SwiftUI

/// Tools ▸ Customize Tools… (⇧⌘T): the strip's tabs, the selected tab's tools,
/// and a palette of every tool. A window, not a sheet: changes apply live, so
/// the sidebar shows the result while you arrange it. The selected tab here is
/// the sidebar's tab.
package struct ToolCustomizeView: View {
    package static let windowID = AppWindows.customizeTools

    @Bindable var model: AppModel
    var layout = ToolLayout.shared
    @Environment(\.dismissWindow) private var dismissWindow
    @State private var tool: ToolPanelID?
    @State private var paletteTool: ToolPanelID?
    @State private var tabName = ""
    /// The tab whose name is being edited (fixed when the field gains focus), so a
    /// selection change mid-edit, here or in the strip, renames the right tab.
    @State private var nameEdit = TabNameEdit()
    @State private var workspaceName = ""
    @FocusState private var focus: Field?

    private enum Field: Hashable { case tabs, tools, palette, tabName, workspaceName }

    package init(model: AppModel, layout: ToolLayout = .shared) {
        self.model = model
        self.layout = layout
    }

    private var tab: ToolTab { ToolWorkspaceCommands.selectedTab(model: model, layout: layout) }
    private var tabTitle: String { layout.workspace.title(of: tab) }

    package var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            HStack(alignment: .top, spacing: 0) {
                tabsColumn.frame(width: 230)
                Divider()
                toolsColumn.frame(maxWidth: .infinity)
                Divider()
                paletteColumn.frame(width: 230)
            }
            Divider()
            footer
        }
        .frame(minWidth: 820, minHeight: 540)
        .background(DS.panelBackground)
        .onAppear {
            tabName = tabTitle
            // A request that opened the window (Save Workspace…, New Tab) wins over the default.
            if !applyFocusRequest() { focus = .tabs }
        }
        .onChange(of: layout.customizeFocus) { _, _ in applyFocusRequest() }
        .onChange(of: tab) { _, _ in
            commitTabName()
            tabName = tabTitle
            tool = nil
            if focus == .tabName { nameEdit.begin(tab, title: tabTitle) }
        }
        .onChange(of: focus) { old, new in
            if old == .tabName, new != .tabName { commitTabName(); tabName = tabTitle }
            if new == .tabName { nameEdit.begin(tab, title: tabTitle) }
        }
        // A load, reset or undo (footer buttons don't take focus) replaces the layout
        // under the field: drop the edit rather than write its stale name back.
        .onChange(of: layout.workspace) { _, _ in
            nameEdit.cancel()
            tabName = tabTitle
            if focus == .tabName { nameEdit.begin(tab, title: tabTitle) }
        }
        // Done, Esc or the close button with the field focused keeps the typed name.
        .onDisappear(perform: commitTabName)
    }

    /// Rename the tab being edited to the field's text.
    private func commitTabName() {
        guard let r = nameEdit.end(tabName) else { return }
        layout.edit { $0.rename(r.tab, to: r.name) }
    }

    @discardableResult
    private func applyFocusRequest() -> Bool {
        guard let request = layout.customizeFocus else { return false }
        layout.customizeFocus = nil
        // Next turn: a window that is just opening has no focusable fields yet.
        Task { @MainActor in focus = request == .tabName ? .tabName : .workspaceName }
        return true
    }

    private var header: some View {
        HStack(spacing: DS.Space.m) {
            Image(systemName: "slider.horizontal.3")
                .font(.system(size: 18))
                .foregroundStyle(DS.accent)
                .frame(width: 36, height: 36)
                .background(DS.accent.opacity(0.14), in: .rect(cornerRadius: DS.Radius.m))
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text("Customize Tools").font(.system(size: 15, weight: .semibold)).foregroundStyle(DS.text)
                Text("Choose the tabs in the strip and the tools in each. A tool can be in several tabs. Changes apply right away.")
                    .font(DS.Font.label).foregroundStyle(DS.textSecondary)
            }
            Spacer()
        }
        .padding(DS.Space.m)
    }

    private func columnTitle(_ text: String) -> some View {
        Text(text.uppercased()).font(DS.Font.panelTitle).tracking(0.6).foregroundStyle(DS.textSecondary)
            .accessibilityAddTraits(.isHeader)
    }

    // MARK: Tabs

    private var tabsColumn: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack {
                columnTitle("Tabs")
                Spacer()
                Menu {
                    Button("New Tab") {
                        ToolWorkspaceCommands.newTab(title: "New Tab", model: model, layout: layout)
                        focus = .tabName
                    }
                    let hidden = layout.workspace.hiddenBuiltIns
                    if !hidden.isEmpty {
                        Divider()
                        ForEach(hidden) { t in
                            Button("Show \(t.title)") { ToolWorkspaceCommands.showTab(t, model: model, layout: layout) }
                        }
                    }
                } label: { Image(systemName: "plus") }
                    .menuIndicator(.hidden).fixedSize()
                    .help("Add a tab").accessibilityLabel("Add a tab")
                iconButton("minus", label: "Remove \(tabTitle) tab") { ToolWorkspaceCommands.removeTab(tab, model: model, layout: layout) }
                    .disabled(!layout.workspace.canRemoveTab(tab))
                iconButton("chevron.up", label: "Move \(tabTitle) tab up") { layout.move(tab, by: -1) }
                    .disabled(!layout.canMove(tab, by: -1))
                iconButton("chevron.down", label: "Move \(tabTitle) tab down") { layout.move(tab, by: 1) }
                    .disabled(!layout.canMove(tab, by: 1))
            }
            List(selection: Binding(get: { Optional(tab) }, set: { if let t = $0 { model.toolTab = t } })) {
                ForEach(layout.order) { t in
                    HStack(spacing: DS.Space.s) {
                        Image(systemName: layout.workspace.symbol(of: t)).frame(width: 18).accessibilityHidden(true)
                        Text(layout.workspace.title(of: t)).foregroundStyle(DS.text)
                        Spacer()
                        Text("\(layout.panels(for: t).count)").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                            .accessibilityLabel("\(layout.panels(for: t).count) tools")
                    }
                    .tag(t)
                    .accessibilityElement(children: .combine)
                    .accessibilityAction(named: "Move Up") { layout.move(t, by: -1) }
                    .accessibilityAction(named: "Move Down") { layout.move(t, by: 1) }
                    .dropDestination(for: ToolDragItem.self) { items, _ in
                        guard let p = items.first?.panel else { return false }
                        layout.edit { $0.insert(p, in: t) }
                        return true
                    }
                }
                .onMove { from, to in layout.edit { $0.moveTabs(fromOffsets: from, toOffset: to) } }
            }
            .focused($focus, equals: .tabs)
            .onDeleteCommand { ToolWorkspaceCommands.removeTab(tab, model: model, layout: layout) }
            .accessibilityLabel("Tabs")
            tabEditor
        }
        .padding(DS.Space.m)
    }

    private var tabEditor: some View {
        VStack(alignment: .leading, spacing: DS.Space.s) {
            TextField("Name", text: $tabName)
                .textFieldStyle(.roundedBorder)
                .focused($focus, equals: .tabName)
                .onSubmit {
                    commitTabName()
                    tabName = tabTitle
                    nameEdit.begin(tab, title: tabTitle) // still focused: further edits go to the same tab
                }
                .accessibilityLabel("Tab name")
            Picker("Icon", selection: Binding(get: { layout.workspace.symbol(of: tab) },
                                              set: { s in layout.edit { $0.setSymbol(tab, s) } })) {
                ForEach(symbols, id: \.self) { s in
                    Label(s, systemImage: s).labelStyle(.iconOnly).tag(s)
                }
            }
            .font(DS.Font.label)
        }
    }

    private var symbols: [String] {
        tab.isCustom || ToolTab.customSymbols.contains(tab.symbol) ? ToolTab.customSymbols : [tab.symbol] + ToolTab.customSymbols
    }

    // MARK: Tools in the tab

    private var toolsColumn: some View {
        let panels = layout.panels(for: tab)
        return VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack {
                columnTitle("Tools in \(tabTitle)")
                Spacer()
                if let tool {
                    iconButton("chevron.up", label: "Move \(tool.title) up") { layout.edit { $0.move(tool, in: tab, by: -1) } }
                        .disabled(!layout.workspace.canMove(tool, in: tab, by: -1))
                        .keyboardShortcut(.upArrow, modifiers: [.command, .option])
                    iconButton("chevron.down", label: "Move \(tool.title) down") { layout.edit { $0.move(tool, in: tab, by: 1) } }
                        .disabled(!layout.workspace.canMove(tool, in: tab, by: 1))
                        .keyboardShortcut(.downArrow, modifiers: [.command, .option])
                    iconButton("minus", label: "Remove \(tool.title) from \(tabTitle)") { remove(tool) }
                }
            }
            List(selection: $tool) {
                if panels.isEmpty {
                    Text("Drag tools here from the list on the right, or select one there and press Return.")
                        .font(DS.Font.label).foregroundStyle(DS.textSecondary)
                }
                ForEach(panels) { p in
                    HStack {
                        Text(p.title).foregroundStyle(DS.text)
                        Spacer()
                        if layout.workspace.tabs(containing: p).count > 1 {
                            Text("also in \(otherTabs(of: p))").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                        }
                    }
                    .tag(p)
                    .accessibilityElement(children: .combine)
                    .accessibilityAction(named: "Move Up") { layout.edit { $0.move(p, in: tab, by: -1) } }
                    .accessibilityAction(named: "Move Down") { layout.edit { $0.move(p, in: tab, by: 1) } }
                    .accessibilityAction(named: "Remove") { remove(p) }
                }
                .onMove { from, to in layout.edit { $0.movePanels(in: tab, fromOffsets: from, toOffset: to) } }
                .onDelete { offsets in offsets.map { panels[$0] }.forEach(remove) }
            }
            .focused($focus, equals: .tools)
            .onDeleteCommand { if let tool { remove(tool) } }
            .dropDestination(for: ToolDragItem.self) { items, _ in
                guard let p = items.first?.panel else { return false }
                layout.edit { $0.insert(p, in: tab) }
                tool = p
                return true
            }
            .accessibilityLabel("Tools in \(tabTitle)")
            Text("⌥⌘↑ / ⌥⌘↓ move the selected tool; Delete removes it. Removed tools stay in the Tools menu.")
                .font(DS.Font.caption).foregroundStyle(DS.textSecondary)
        }
        .padding(DS.Space.m)
    }

    private func otherTabs(of p: ToolPanelID) -> String {
        layout.workspace.tabs(containing: p).filter { $0 != tab }.map { layout.workspace.title(of: $0) }.joined(separator: ", ")
    }

    private func remove(_ p: ToolPanelID) {
        layout.edit { $0.remove(p, from: tab) }
        if tool == p { tool = nil }
    }

    // MARK: Palette

    private var paletteColumn: some View {
        let inTab = layout.panels(for: tab)
        return VStack(alignment: .leading, spacing: DS.Space.s) {
            HStack {
                columnTitle("All Tools")
                Spacer()
                Button("Add") { if let paletteTool { add(paletteTool) } }
                    .disabled(paletteTool.map(inTab.contains) ?? true)
                    .help("Add the selected tool to \(tabTitle) (Return, or double-click)")
            }
            List(selection: $paletteTool) {
                ForEach(ToolWorkspaceCommands.menuSections, id: \.tab) { section in
                    Section(section.tab.title) {
                        ForEach(section.panels) { p in
                            HStack {
                                Text(p.title).foregroundStyle(DS.text)
                                Spacer()
                                if inTab.contains(p) {
                                    Image(systemName: "checkmark").font(DS.Font.caption).foregroundStyle(DS.textSecondary)
                                }
                            }
                            .tag(p)
                            .accessibilityElement(children: .combine)
                            .accessibilityValue(inTab.contains(p) ? "In \(tabTitle)" : "")
                            .accessibilityAction(named: "Add to \(tabTitle)") { add(p) }
                            .draggable(ToolDragItem.panel(p))
                        }
                    }
                }
            }
            .focused($focus, equals: .palette)
            .contextMenu(forSelectionType: ToolPanelID.self) { sel in
                Button("Add to \(tabTitle)") { sel.forEach(add) }
            } primaryAction: { sel in
                sel.forEach(add)
            }
            .accessibilityLabel("All tools")
        }
        .padding(DS.Space.m)
    }

    private func add(_ p: ToolPanelID) {
        layout.edit { $0.insert(p, in: tab) }
        tool = p
    }

    // MARK: Footer

    private var footer: some View {
        HStack(spacing: DS.Space.s) {
            Menu("Workspaces") {
                if layout.saved.isEmpty { Text("No saved workspaces") }
                ForEach(layout.saved, id: \.name) { w in
                    Button(w.name) { ToolWorkspaceCommands.loadWorkspace(named: w.name, model: model, layout: layout) }
                }
                if !layout.saved.isEmpty {
                    Divider()
                    Menu("Delete Workspace") {
                        ForEach(layout.saved, id: \.name) { w in
                            Button(w.name) { layout.deleteWorkspace(named: w.name) }
                        }
                    }
                }
                Divider()
                Button("Undo Workspace Change") { ToolWorkspaceCommands.undoWorkspaceChange(model: model, layout: layout) }
                    .disabled(layout.undoWorkspace == nil)
            }
            .fixedSize()
            TextField("Workspace name", text: $workspaceName)
                .textFieldStyle(.roundedBorder)
                .frame(width: 180)
                .focused($focus, equals: .workspaceName)
                .onSubmit(saveWorkspace)
                .accessibilityLabel("Workspace name")
            Button("Save Workspace", action: saveWorkspace)
                .disabled(ToolLayout.cleanName(workspaceName) == nil)
                .help("Save these tabs, tools and collapsed panels under this name (an existing one is replaced)")
            Spacer()
            Button("Reset to Default") { ToolWorkspaceCommands.resetToDefault(model: model, layout: layout) }
                .help("The built-in tabs and tools (Workspaces ▸ Undo Workspace Change brings yours back)")
            // Esc, not Return: Return adds the palette's selected tool and submits the name fields.
            Button("Done") {
                commitTabName()
                dismissWindow(id: Self.windowID)
            }
                .keyboardShortcut(.cancelAction)
        }
        .padding(DS.Space.m)
    }

    private func saveWorkspace() {
        if ToolWorkspaceCommands.saveWorkspace(named: workspaceName, model: model, layout: layout) { workspaceName = "" }
    }

    private func iconButton(_ symbol: String, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) { Image(systemName: symbol).frame(width: 16, height: 16) }
            .buttonStyle(.borderless)
            .help(label)
            .accessibilityLabel(label)
    }
}
