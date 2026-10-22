import SwiftUI
import UniformTypeIdentifiers

/// Accepts photos/folders dragged onto the window. Routing is in `DropRouter`.
/// Usage: `.modifier(PhotoDropTarget(model: model))`.
struct PhotoDropTarget: ViewModifier {
    let model: AppModel
    @State private var targeted = false

    func body(content: Content) -> some View {
        content
            .onDrop(of: [.fileURL], isTargeted: $targeted) { providers in
                load(providers)
                return !providers.isEmpty
            }
            .overlay {
                if targeted {
                    RoundedRectangle(cornerRadius: DS.Radius.l)
                        .strokeBorder(DS.accent, style: StrokeStyle(lineWidth: 3, dash: [10, 6]))
                        .padding(DS.Space.s)
                        .overlay(
                            Label(model.mode == .live ? "Drop to browse this folder" : "Drop to import",
                                  systemImage: "square.and.arrow.down")
                                .font(.headline)
                                .padding(DS.Space.m)
                                .glassEffect(.regular, in: .capsule)
                        )
                        .allowsHitTesting(false)
                }
            }
    }

    private func load(_ providers: [NSItemProvider]) {
        guard let provider = providers.first(where: { $0.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) })
        else { return }
        _ = provider.loadObject(ofClass: URL.self) { url, _ in
            guard let url else { return }
            Task { @MainActor in perform(url) }
        }
    }

    @MainActor private func perform(_ url: URL) {
        let opener = (model as AnyObject) as? any FolderOpening
        switch DropRouter.route([url], mode: model.mode, canOpenFolders: opener != nil) {
        case .openFolder(let folder):
            Task { await opener?.openFolder(folder) }
        case .importSheet(let folder):
            model.sheet = .importCard(folder)
        case .ignore:
            break
        }
    }
}
