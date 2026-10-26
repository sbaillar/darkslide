import LuxtableCore
import Foundation

/// The folder panel behind Open Folder… / Choose Folder….
package enum OpenFolderPanel {
    @MainActor
    package static func run(model: AppModel) {
        guard let url = PlatformPanels.chooseItem(files: false, directories: true,
                                                  message: "Choose a folder of RAW files to browse") else { return }
        Task { await model.openFolder(url) }
    }
}
