import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@MainActor
@Suite struct StackNavigationTests {
    @Test func collapsedStacksAreWalkedOnce() async {
        let m = makeModel(counts: [60])
        await m.start()
        m.browserState.refresh(photos: m.photos)
        let stacked = m.visibleIDs.first { (m.browserState.stacks.stack(of: $0)?.count ?? 0) > 1 }
        guard let member = stacked else {
            Issue.record("mock data has no bursts")
            return
        }
        m.focus(member, extend: false)
        m.perform(.toggleStack)
        #expect(m.browserState.stackBursts)
        let stacks = m.browserState.stacks
        let stack = stacks.stack(of: member)!
        #expect(stacks.isCollapsed(stack), "turning stacking on collapses bursts")
        let hidden = stacks.stacks.filter { stacks.isCollapsed($0) }.reduce(0) { $0 + $1.count - 1 }
        let nav = m.navigationIDs
        #expect(nav.count == m.visibleIDs.count - hidden && hidden > 0)
        m.perform(.toggleStack)
        #expect(!stacks.isCollapsed(stack), "second S expands the focused stack")
        #expect(m.navigationIDs.count == nav.count + stack.count - 1)
        m.perform(.toggleStack)
        m.focus(nav[0], extend: false)
        for _ in 0..<nav.count { m.perform(.move(.right, extend: false)) }
        #expect(m.focusedID == nav.last)
        m.perform(.move(.left, extend: false))
        #expect(m.focusedID == nav[nav.count - 2])
        #expect(m.photoCount == 60)
        m.focus(nil, extend: false)
        m.perform(.toggleStack) // no focus: no-op
        #expect(m.navigationIDs.count == nav.count)
    }
}
