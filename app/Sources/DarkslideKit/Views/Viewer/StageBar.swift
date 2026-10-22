import SwiftUI

/// The tools that act on the photo, floating over the top of the viewer: Before/After,
/// Compare, Selection (multi-view), Crop and Straighten. Out of the toolbar so the window's chrome stays short
/// and they sit next to what they change.
struct StageBar: View {
    @Bindable var model: AppModel

    var body: some View {
        let keys = model.settings.keyBindings
        HStack(spacing: DS.Space.xs) {
            Button { model.perform(.toggleBeforeAfter) } label: {
                Label("Before/After", systemImage: "square.split.2x1")
            }
            .buttonStyle(ToolbarPillStyle(showsTitle: true, isOn: model.viewer.state.beforeAfter))
            .disabled(model.focusedID == nil)
            .help(ToolbarHelp.beforeAfter(keys))
            .accessibilityLabel(ToolbarControl.beforeAfter.control(model).label)
            Button { model.viewer.togglePinCompare() } label: {
                Label("Compare", systemImage: model.viewer.state.comparePinned == nil ? "pin" : "pin.fill")
            }
            .buttonStyle(ToolbarPillStyle(showsTitle: true, isOn: model.viewer.state.comparePinned != nil))
            .disabled(model.focusedID == nil)
            .help(ToolbarHelp.compare(keys))
            .accessibilityLabel(ToolbarControl.compare.control(model).label)
            Button { // [multiview]
                CommandCatalog.perform(.showSelectionInViewer, model: model)
            } label: {
                Label("Selection", systemImage: "rectangle.split.2x2")
            }
            .buttonStyle(ToolbarPillStyle(showsTitle: true, isOn: model.settings.showSelectionInViewer))
            .help(ToolbarHelp.showSelection)
            .accessibilityLabel(ToolbarControl.showSelection.control(model).label)
            Divider().frame(height: 16)
            // [cmp9] Composition tools: crop and straighten.
            ForEach([CanvasTool.crop, .straighten], id: \.self) { tool in
                Button { model.perform(.tool(tool)) } label: {
                    Label(tool.viewerLabel, systemImage: tool.viewerSymbol)
                }
                .buttonStyle(ToolbarPillStyle(showsTitle: true, isOn: model.viewer.canvasTool == tool))
                .disabled(model.focusedPhoto.map { $0.isVideo } ?? true)
                .help(ToolbarHelp.tool(tool, keys))
                .accessibilityLabel("\(tool.viewerLabel) tool")
            }
        }
        .padding(DS.Space.xs)
        .background(DS.chrome.opacity(0.94), in: .capsule) // solid: legible over any photo
        .overlay(Capsule().strokeBorder(DS.hairline))
        .fixedSize()
        .padding(.top, DS.Space.s)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Photo tools")
    }
}
