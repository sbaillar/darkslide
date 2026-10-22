import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@Suite struct AccessibilityLabelTests {
    @Test func photoLabels() {
        var p = samplePhoto(1)
        #expect(PhotoAccessibility.label(p) == "DSC01.ARW, unrated")
        p.rating = 1
        p.pick = .picked
        p.colorTag = .green
        p.isEdited = true
        #expect(PhotoAccessibility.label(p, stackCount: 4) == "DSC01.ARW, 1 star, picked, green tag, adjusted, stack of 4 photos")
        p.rating = 3
        p.pick = .rejected
        #expect(PhotoAccessibility.label(p).contains("3 stars, rejected"))
        #expect(PhotoAccessibility.isSelected(false, true) && !PhotoAccessibility.isSelected(false, false))
    }
}
