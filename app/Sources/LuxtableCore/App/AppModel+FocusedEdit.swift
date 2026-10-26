import Foundation

/// The focused image's edit (identity when nothing is focused).
extension AppModel {
    package var focusedEdit: EditState { focusedID.map { edit(for: $0) } ?? EditState() }
}
