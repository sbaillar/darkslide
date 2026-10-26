import LuxtableCore
import SwiftUI

/// The rawler helper's download, in the viewer's banner: progress while it runs, the
/// reason when it failed, nothing otherwise.
package struct RawlerHelperProgress: View {
    var helper: RawlerHelper

    package init(helper: RawlerHelper) {
        self.helper = helper
    }

    package var body: some View {
        switch helper.state {
        case .downloading(let f):
            ProgressView(value: f).frame(width: 80).controlSize(.small)
                .accessibilityLabel(RawlerHelperText.status(helper.state))
        case .failed(let m):
            Text(m).font(DS.Font.label).foregroundStyle(DS.reject)
        case .notInstalled, .installed:
            EmptyView()
        }
    }
}
