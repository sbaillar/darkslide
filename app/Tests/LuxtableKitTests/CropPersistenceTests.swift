import CoreGraphics
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

@MainActor
@Suite struct CropPersistenceTests {
    @Test func rotationAndCropLiveInTheEditAndUndo() async {
        let m = makeModel()
        await m.start()
        m.viewer.rotate(by: 1)
        #expect(m.focusedEdit.crop.quarterTurns == 1 && m.viewer.rotation == 1)
        m.viewer.rotate(by: -1)
        m.viewer.rotate(by: -1)
        #expect(m.focusedEdit.crop.quarterTurns == 3)
        m.undo()
        #expect(m.focusedEdit.crop.quarterTurns == 0)
        #expect(m.viewer.cropRect == CGRect(x: 0, y: 0, width: 1, height: 1))
        // Crop is copyable like any other group.
        m.updateEdit(label: "crop") { $0.crop.rect = CGRect(x: 0.1, y: 0.1, width: 0.5, height: 0.5) }
        m.copyAdjustments()
        let other = m.visibleIDs[3]
        m.applyClipboard(to: [other], options: PasteOptions(groups: [.crop]))
        #expect(m.edit(for: other).crop.rect == CGRect(x: 0.1, y: 0.1, width: 0.5, height: 0.5))
        m.viewer.canvasTool = .crop
        m.perform(.escape)
        #expect(m.viewer.canvasTool == .pan, "Esc leaves the canvas tool")
    }
}
